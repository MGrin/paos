//! A ranking gate that runs in CI, on a store built here.
//!
//! `rank-bench` is the real measurement and it needs a real corpus and the 129 MB
//! embedding model, so it cannot run on a fresh checkout. This is the part that CAN:
//! ranking is several signals combined, and each can be disconnected in a way no unit
//! test on its own components notices. Ranking was changed three times in one day with
//! nothing watching, which is what this is for.
//!
//! EVERY FIXTURE HERE IS BUILT TO FAIL WHEN ITS SIGNAL IS REMOVED, and the first draft of
//! this file was not. It asserted things that were already true: with two facts and a
//! bag-of-words embedder, the "right" one won on plain similarity, so deleting the alias
//! term AND flattening the usefulness multiplier left all four tests green. A guard that
//! cannot fail is worse than no guard, because it is also a claim. The distractors below
//! are deliberately closer to the query than the target, so only the signal under test
//! can separate them — verified by mutating the source and watching each one go red.
//!
//! WHAT THIS DOES NOT PROVE. It runs on `HashEmbedder`, so it says nothing about semantic
//! quality; a change that made real retrieval worse could still pass. And the lexical
//! blend has no test here on purpose: `HashEmbedder` IS a bag of words, so its dense
//! signal and the lexical term agree by construction and no fixture can separate them.
//! Gating that one needs the real embedder, which means `rank-bench`.

use paos_memory::{ensure_schema, recall, remember, set_aliases, Embedder, HashEmbedder};
use rusqlite::Connection;

fn store() -> (Connection, HashEmbedder) {
    let c = Connection::open_in_memory().unwrap();
    ensure_schema(&c).unwrap();
    (c, HashEmbedder::new(256))
}

fn top(c: &Connection, e: &dyn Embedder, q: &str) -> String {
    recall(c, e, &["ds".to_string()], q, 1)
        .unwrap()
        .first()
        .map(|h| h.memory.text.clone())
        .unwrap_or_default()
}

#[test]
fn a_phrasing_beats_a_fact_that_merely_shares_the_questions_words() {
    // The distractor carries four of the query's five words and the target carries none,
    // so the phrasing is the ONLY thing that can win this. Remove the alias term from
    // recall's scoring and the distractor takes first place.
    let (c, e) = store();
    let id = remember(&c, &e, "ds", "kettle boils at one hundred", "2026-08-01").unwrap();
    remember(&c, &e, "ds", "how hot the office gets in summer is a running complaint",
             "2026-08-01").unwrap();
    set_aliases(&c, &e, &id, Some("how hot does water get")).unwrap();
    assert_eq!(top(&c, &e, "how hot does water get"), "kettle boils at one hundred");
}

#[test]
fn a_fact_that_has_earned_its_place_outranks_an_identical_one() {
    // The distractor is an EXACT match for the query and the target is not, so the target
    // can only win by having earned it. An earlier draft used two anagrams, assuming a
    // bag-of-words embedder would score them identically and leave usefulness to decide;
    // it did not, and the test passed with the multiplier flattened to a constant.
    let (c, e) = store();
    let used = remember(&c, &e, "ds", "alpha deploy notes", "2026-08-01").unwrap();
    remember(&c, &e, "ds", "alpha deploy", "2026-08-01").unwrap();
    for _ in 0..40 {
        paos_memory::reinforce(&c, &[&used]).unwrap();
    }
    assert_eq!(top(&c, &e, "alpha deploy"), "alpha deploy notes");
}

#[test]
fn clearing_phrasings_gives_the_ranking_back() {
    // Reversibility as a RANKING property, not a storage one. `--clear` is the promise
    // that a bad phrasings pass can be undone; if the vector outlived the column, the
    // store would keep answering from phrasings it no longer admits to having.
    let (c, e) = store();
    let id = remember(&c, &e, "ds", "kettle boils at one hundred", "2026-08-01").unwrap();
    remember(&c, &e, "ds", "how hot the office gets in summer is a running complaint",
             "2026-08-01").unwrap();
    set_aliases(&c, &e, &id, Some("how hot does water get")).unwrap();
    set_aliases(&c, &e, &id, None).unwrap();
    assert_ne!(top(&c, &e, "how hot does water get"), "kettle boils at one hundred");
}

#[test]
fn scope_still_cannot_leak() {
    // Not a ranking property but a containment one, and it belongs in the gate that runs
    // on every change to recall because it is the failure with the worst blast radius: a
    // work fact surfacing in a personal repo.
    let (c, e) = store();
    remember(&c, &e, "other", "the production database password rotates monthly", "2026-08-01")
        .unwrap();
    remember(&c, &e, "ds", "something else entirely", "2026-08-01").unwrap();
    let hits = recall(&c, &e, &["ds".to_string()], "production database password", 5).unwrap();
    assert!(hits.iter().all(|h| h.memory.dataset == "ds"), "recall left its scope");
}
