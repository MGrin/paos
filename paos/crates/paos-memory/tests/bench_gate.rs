//! A retrieval-quality floor that runs on a clean checkout.
//!
//! `tests/ranking.rs` proves each SIGNAL is still wired in. This proves the pipeline still
//! WORKS end to end: a fixed corpus, a fixed set of questions asked in different words
//! from the facts that answer them, and a floor on how many come back first.
//!
//! WHAT IT IS NOT. It runs on `HashEmbedder`, so it is not a claim about semantic quality
//! — the real measurement is `rank-bench` against a real store with the real model, and
//! that needs a 129 MB download and a corpus this repo does not have. What this catches is
//! a change that breaks scoring for everyone: a blend inverted, a multiplier dropped, a
//! dedup that eats results, an ordering reversed. Those show up as the number falling.
//!
//! THE FLOOR IS SET BELOW THE CURRENT SCORE ON PURPOSE. A threshold pinned to today's
//! exact number fails on every harmless change and gets raised until nobody trusts it. It
//! is set to catch a collapse, not a wobble.
//!
//! WHAT IT WAS VERIFIED TO CATCH, by breaking the source and watching it:
//!
//!   final ordering reversed ......... hit@1 5/15 -> 1/15, RED
//!   cosine forced to zero ........... still GREEN
//!
//! The second one is the honest limit and it is worth stating plainly. With the lexical
//! blend at 0.5 and a bag-of-words embedder, either signal alone still ranks this corpus:
//! killing the dense half leaves the lexical half doing the whole job, and the gate never
//! notices. It catches ordering, deduplication, scope and total collapse. It does NOT
//! catch one of the two signals dying — that needs the real embedder, where the two stop
//! agreeing, which means `rank-bench`.

use paos_memory::{ensure_schema, recall, remember, Embedder, HashEmbedder};
use rusqlite::Connection;

/// (dataset, fact) — three datasets so scope filtering is exercised, and several facts per
/// topic so the right answer has to beat plausible neighbours rather than empty space.
const CORPUS: &[(&str, &str)] = &[
    ("ops", "the deploy script refuses to run when the working tree is dirty"),
    ("ops", "deploys happen from the release branch only, never from a feature branch"),
    ("ops", "a rollback restores the previous container image but not the database"),
    ("ops", "the staging cluster is rebuilt every night and loses any manual change"),
    ("ops", "log retention is fourteen days, after which nothing is recoverable"),
    ("ops", "the health endpoint reports ready before migrations have finished"),
    ("ops", "certificates renew automatically sixty days before they expire"),
    ("ops", "the metrics agent samples once a minute, so short spikes are invisible"),
    ("app", "uploads larger than ten megabytes are rejected by the proxy, not the app"),
    ("app", "the session cookie is signed but not encrypted, so never put secrets in it"),
    ("app", "search results are cached for five minutes and ignore permission changes"),
    ("app", "the export runs in a worker, so a browser refresh does not cancel it"),
    ("app", "timestamps are stored in UTC and converted only when rendering"),
    ("app", "a failed payment retries three times before the order is cancelled"),
    ("app", "the admin panel is behind a separate login with its own password policy"),
    ("app", "feature flags are read once at boot, so a change needs a restart"),
    ("data", "the nightly import skips rows whose primary key already exists"),
    ("data", "currency amounts are integers in minor units, never floats"),
    ("data", "the warehouse copy lags production by up to an hour"),
    ("data", "deleted records are marked, not removed, so counts include them"),
    ("data", "the report joins on email, which is not unique across tenants"),
    ("data", "backfills run with a lower priority and can take a whole weekend"),
];

/// (dataset, question, substring identifying the right fact) — the questions deliberately
/// avoid the fact's own distinctive words, so a match is not a tautology.
const GOLDEN: &[(&str, &str, &str)] = &[
    ("ops", "why will it not let me ship with uncommitted changes", "dirty"),
    ("ops", "can I release straight from the branch I am working on", "release branch"),
    ("ops", "going back a version did not bring my rows back", "rollback"),
    ("ops", "my hand-made change to the test environment disappeared", "rebuilt every night"),
    ("ops", "how far back can I read what happened", "retention"),
    ("ops", "traffic arrived before the schema was ready", "migrations"),
    ("app", "a big file was refused before reaching my code", "proxy"),
    ("app", "is it safe to keep a token in the browser cookie", "signed but not encrypted"),
    ("app", "someone still saw a document after I revoked access", "cached"),
    ("app", "will closing the tab stop the long download", "worker"),
    ("data", "money is coming out wrong by a factor of a hundred", "minor units"),
    ("app", "changing a toggle did nothing until much later", "read once at boot"),
    ("data", "the numbers do not match production", "lags production"),
    ("data", "my totals include things I got rid of", "marked, not removed"),
    ("data", "two customers' rows are being mixed together", "not unique"),
];

/// hit@1 must stay at or above this.
///
/// The measured score on this corpus is printed by the test when it fails; the floor sits
/// well under it. It is low in absolute terms because `HashEmbedder` is a bag of words and
/// these questions deliberately avoid their answer's vocabulary — the honest response to
/// that is a floor that catches a collapse, not questions rewritten until the number looks
/// impressive. A gate that flatters itself is worse than none.
const HIT1_FLOOR: usize = 3;
/// MRR must stay at or above this.
const MRR_FLOOR: f64 = 0.30;

fn store() -> (Connection, HashEmbedder) {
    let c = Connection::open_in_memory().unwrap();
    ensure_schema(&c).unwrap();
    let e = HashEmbedder::new(512);
    for (ds, text) in CORPUS {
        remember(&c, &e, ds, text, "2026-08-01").unwrap();
    }
    (c, e)
}

fn score(c: &Connection, e: &dyn Embedder) -> (usize, f64) {
    let (mut hit1, mut mrr) = (0usize, 0.0f64);
    for (ds, question, needle) in GOLDEN {
        let hits = recall(c, e, &[ds.to_string()], question, 5).unwrap_or_default();
        if let Some(p) = hits.iter().position(|h| h.memory.text.contains(needle)) {
            if p == 0 {
                hit1 += 1;
            }
            mrr += 1.0 / (p + 1) as f64;
        }
    }
    (hit1, mrr / GOLDEN.len() as f64)
}

#[test]
fn retrieval_quality_has_not_collapsed() {
    let (c, e) = store();
    let (hit1, mrr) = score(&c, &e);
    assert!(
        hit1 >= HIT1_FLOOR,
        "hit@1 {hit1}/{} is below the floor of {HIT1_FLOOR} — recall scoring regressed",
        GOLDEN.len()
    );
    assert!(mrr >= MRR_FLOOR, "MRR {mrr:.3} is below the floor of {MRR_FLOOR}");
    // Printed so a reader can see the headroom without editing the test — a floor whose
    // distance from the real score is invisible gets raised to the score sooner or later,
    // and then it fails on every harmless change.
    println!("bench gate: hit@1 {hit1}/{} MRR {mrr:.3}", GOLDEN.len());
}

#[test]
fn the_questions_do_not_simply_quote_their_answers() {
    // The floor above is only worth something if the questions are hard. If a question
    // shared most of its words with its fact, ANY scoring would pass and the gate would be
    // a tautology that reads as a quality bar.
    for (_, question, needle) in GOLDEN {
        let q: Vec<&str> = question.split_whitespace().collect();
        let shared = q.iter().filter(|w| needle.contains(**w)).count();
        assert!(
            (shared as f64) < q.len() as f64 * 0.5,
            "question {question:?} shares too much with {needle:?} to prove anything"
        );
    }
}

#[test]
fn every_golden_answer_is_actually_in_the_corpus() {
    // A needle matching nothing would make its case an unfixable miss, quietly lowering
    // the ceiling and making the floor easier to clear the day someone raises it.
    for (ds, question, needle) in GOLDEN {
        assert!(
            CORPUS.iter().any(|(d, t)| d == ds && t.contains(needle)),
            "no fact in {ds} contains {needle:?} (for {question:?})"
        );
    }
}
