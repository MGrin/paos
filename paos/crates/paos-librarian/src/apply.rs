//! Applying an approved proposal to the memory store.
//!
//! Every memory write here goes through the daemon. Two reasons, both of which fail
//! SILENTLY if ignored: the daemon is the single writer, and it computes embeddings with
//! the model the store was built with. A local write would race AND land vectors in a
//! different space, which looks fine until recall quietly stops matching.
//!
//! The apply path is expressed as a PLAN — a list of steps — separately from executing
//! it. That split is what lets the ordering requirements be tested directly: a test can
//! assert that a split's retirement comes last without needing a daemon, and it FAILS if
//! the order is swapped rather than passing by accident because the end state happened to
//! look right.

use crate::queue::{Proposal, SPLIT_SEP};

/// One step of an apply.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Plain write, no retirement.
    Store { dataset: String, text: String },
    /// Store `text` AND retire every id in `old_ids`, atomically, under one daemon lock.
    StoreAndRetire { dataset: String, text: String, old_ids: Vec<String> },
    /// Retire facts with nothing replacing them.
    ///
    /// The only step that removes without adding, and the only one whose mistake cannot
    /// be spotted by reading what it wrote — there is nothing to read. That is why the
    /// pass that produces it is the most conservative one here and why retirement sets
    /// `superseded` rather than deleting: an approval that turns out wrong is one UPDATE
    /// away from being undone.
    Retire { old_ids: Vec<String> },
}

/// Why a proposal cannot be applied.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// Not pending — already approved or rejected.
    NotPending,
    /// Every source it names is gone. The proposal is RETIRED, not applied.
    ///
    /// Applying it would RESURRECT deleted content: a split of an obsolete entry,
    /// approved after the entry was deleted, puts its pieces straight back into memory.
    AllSourcesGone,
    /// A split whose text does not actually contain two parts. Refuse rather than
    /// silently rewrite the original.
    NotASplit,
    /// A supersede or tidy with no replacement text would retire facts and put nothing in
    /// their place.
    EmptyReplacement,
    /// A kind this code does not know how to apply.
    UnknownKind(String),
}

/// Build the ordered steps for a proposal.
///
/// `fact_exists` decides which named sources are still present. It is a parameter so the
/// resurrection guard is testable without a store, and so it can FAIL OPEN in production
/// (an unreadable database must not convert every pending proposal into a rejection).
pub fn plan_apply<F>(p: &Proposal, fact_exists: F) -> Result<Vec<Step>, Refusal>
where
    F: Fn(&str) -> bool,
{
    if p.status != "pending" {
        return Err(Refusal::NotPending);
    }
    let text = p.text.as_deref().unwrap_or("").trim().to_string();
    let targets: Vec<String> = p
        .target_data_id
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect();

    let targets = if targets.is_empty() {
        targets
    } else {
        let alive: Vec<String> = targets.into_iter().filter(|t| fact_exists(t)).collect();
        if alive.is_empty() {
            return Err(Refusal::AllSourcesGone);
        }
        alive
    };

    let ds = p.dataset.clone();
    match p.kind.as_str() {
        // A lesson is a capture with better provenance and applies the same way. It stays
        // a distinct KIND so its approval rate is measurable separately from dream's,
        // which currently rejects 85% of what it proposes.
        "capture" | "lesson" => Ok(vec![Step::Store { dataset: ds, text }]),
        "split" => {
            let parts: Vec<String> = text
                .split(SPLIT_SEP)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            if parts.len() < 2 {
                return Err(Refusal::NotASplit);
            }
            // EVERY part but the last is stored plainly; the LAST one carries the
            // retirement. Nothing is retired until every part is safely stored, so a
            // daemon that dies mid-split leaves the original intact and the proposal
            // pending. Swapping this order means a crash can delete the original with
            // only some of its replacements written.
            let mut steps: Vec<Step> = parts[..parts.len() - 1]
                .iter()
                .map(|part| Step::Store { dataset: ds.clone(), text: part.clone() })
                .collect();
            steps.push(Step::StoreAndRetire {
                dataset: ds,
                text: parts[parts.len() - 1].clone(),
                old_ids: targets,
            });
            Ok(steps)
        }
        // RETIRE the sources, never delete them. This used to store-then-hard-DELETE, so
        // every approved merge destroyed the facts it merged, and `memories.superseded` —
        // the column added for exactly this, honoured by every reader — stayed empty
        // across 1051 facts. A cleanup that cannot be audited is indistinguishable from
        // data loss.
        // A resolved contradiction IS a supersede: store what is now true and retire
        // what it refutes. Sharing the path is the point — the ordering that keeps a
        // failed write from costing the original is the same ordering, and a second
        // implementation of it would be a second chance to get it wrong.
        "supersede" | "tidy" | "contradiction" => {
            if text.is_empty() {
                return Err(Refusal::EmptyReplacement);
            }
            Ok(vec![Step::StoreAndRetire { dataset: ds, text, old_ids: targets }])
        }
        // No text by design: a retirement proposes removing a fact, not replacing it, so
        // the emptiness that refuses a supersede is the normal state here.
        "retire" => {
            if targets.is_empty() {
                return Err(Refusal::AllSourcesGone);
            }
            Ok(vec![Step::Retire { old_ids: targets }])
        }
        other => Err(Refusal::UnknownKind(other.to_string())),
    }
}

/// Whether a refusal should RETIRE the proposal rather than leave it pending.
///
/// Only `AllSourcesGone`. The others are conditions a human can still act on, and
/// silently rejecting them would hide a malformed proposal instead of showing it.
pub fn refusal_retires(r: &Refusal) -> bool {
    matches!(r, Refusal::AllSourcesGone)
}

#[cfg(test)]
mod tests {
    use super::*;


    fn proposal(kind: &str, text: Option<&str>, targets: Option<&str>) -> Proposal {
        Proposal {
            id: 1,
            kind: kind.into(),
            dataset: "ds".into(),
            scope: None,
            text: text.map(str::to_string),
            target_data_id: targets.map(str::to_string),
            rationale: None,
            source: None,
            status: "pending".into(),
            created_ts: "T".into(),
            resolved_ts: None,
            screen: None,
            screen_why: None,
        }
    }

    fn all_alive(_: &str) -> bool {
        true
    }
    fn none_alive(_: &str) -> bool {
        false
    }

    #[test]
    fn a_retirement_needs_no_replacement_text() {
        // Every other kind refuses on empty text because it would retire facts and put
        // nothing in their place. For a retirement that IS the intent, so the guard that
        // protects the others must not fire here.
        assert_eq!(
            plan_apply(&proposal("retire", None, Some("f1")), all_alive).unwrap(),
            vec![Step::Retire { old_ids: vec!["f1".into()] }]
        );
    }

    #[test]
    fn a_retirement_whose_target_is_already_gone_is_refused() {
        // Not merely pointless — the resurrection guard exists because approving a stale
        // proposal can undo a deletion, and a retirement of nothing should retire the
        // PROPOSAL rather than sit in the queue forever.
        assert_eq!(
            plan_apply(&proposal("retire", None, Some("gone")), none_alive),
            Err(Refusal::AllSourcesGone)
        );
    }

    #[test]
    fn a_capture_is_a_single_plain_store() {
        let steps = plan_apply(&proposal("capture", Some("a fact"), None), all_alive).unwrap();
        assert_eq!(steps, vec![Step::Store { dataset: "ds".into(), text: "a fact".into() }]);
    }

    #[test]
    fn a_lesson_applies_exactly_like_a_capture() {
        let a = plan_apply(&proposal("capture", Some("x"), None), all_alive).unwrap();
        let b = plan_apply(&proposal("lesson", Some("x"), None), all_alive).unwrap();
        assert_eq!(a, b, "the kind is for measurement, not for a different apply");
    }

    #[test]
    fn a_tidy_retires_every_source_it_merges() {
        let steps = plan_apply(&proposal("tidy", Some("merged"), Some("a,b,c")), all_alive)
            .unwrap();
        assert_eq!(
            steps,
            vec![Step::StoreAndRetire {
                dataset: "ds".into(),
                text: "merged".into(),
                old_ids: vec!["a".into(), "b".into(), "c".into()],
            }],
            "a merge that retires only the first source leaves the rest live"
        );
    }

    #[test]
    fn a_split_stores_every_part_before_anything_is_retired() {
        // THE ordering requirement. This test fails if the retirement is moved earlier.
        let text = format!("one{SPLIT_SEP}two{SPLIT_SEP}three");
        let steps = plan_apply(&proposal("split", Some(&text), Some("orig")), all_alive)
            .unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0], Step::Store { dataset: "ds".into(), text: "one".into() });
        assert_eq!(steps[1], Step::Store { dataset: "ds".into(), text: "two".into() });
        assert_eq!(
            steps[2],
            Step::StoreAndRetire {
                dataset: "ds".into(),
                text: "three".into(),
                old_ids: vec!["orig".into()],
            }
        );
        // Stated as an invariant rather than only as positions, so a refactor that
        // reorders the vector still trips it.
        let retire_at = steps.iter().position(|s| matches!(s, Step::StoreAndRetire { .. }));
        assert_eq!(retire_at, Some(steps.len() - 1),
                   "retirement MUST be last: a crash before it leaves the original intact");
        assert_eq!(steps.iter().filter(|s| matches!(s, Step::StoreAndRetire { .. })).count(),
                   1, "exactly one step may retire");
    }

    #[test]
    fn a_two_part_split_still_stores_before_retiring() {
        let text = format!("one{SPLIT_SEP}two");
        let steps = plan_apply(&proposal("split", Some(&text), Some("orig")), all_alive)
            .unwrap();
        assert!(matches!(steps[0], Step::Store { .. }));
        assert!(matches!(steps[1], Step::StoreAndRetire { .. }));
    }

    #[test]
    fn a_split_with_one_part_is_refused_rather_than_rewriting_the_original() {
        assert_eq!(
            plan_apply(&proposal("split", Some("just one"), Some("orig")), all_alive),
            Err(Refusal::NotASplit)
        );
    }

    #[test]
    fn a_proposal_whose_sources_are_all_gone_is_retired_not_applied() {
        // Applying it would RESURRECT deleted content.
        let text = format!("one{SPLIT_SEP}two");
        let r = plan_apply(&proposal("split", Some(&text), Some("orig")), none_alive);
        assert_eq!(r, Err(Refusal::AllSourcesGone));
        assert!(refusal_retires(&r.unwrap_err()), "retire the proposal, do not re-show it");
    }

    #[test]
    fn a_proposal_with_one_surviving_source_still_applies_to_that_one() {
        let alive = |id: &str| id == "b";
        let steps = plan_apply(&proposal("tidy", Some("merged"), Some("a,b,c")), alive)
            .unwrap();
        assert_eq!(
            steps,
            vec![Step::StoreAndRetire {
                dataset: "ds".into(),
                text: "merged".into(),
                old_ids: vec!["b".into()],
            }],
            "the dead sources are dropped, the live one is still retired"
        );
    }

    #[test]
    fn an_empty_replacement_never_retires_anything() {
        // Retiring facts and putting nothing in their place is data loss with extra steps.
        for kind in ["supersede", "tidy"] {
            assert_eq!(
                plan_apply(&proposal(kind, Some("   "), Some("a")), all_alive),
                Err(Refusal::EmptyReplacement)
            );
            assert_eq!(
                plan_apply(&proposal(kind, None, Some("a")), all_alive),
                Err(Refusal::EmptyReplacement)
            );
        }
    }

    #[test]
    fn only_all_sources_gone_retires_the_proposal() {
        // The others are conditions a human can still act on; auto-rejecting them would
        // hide a malformed proposal instead of showing it.
        assert!(!refusal_retires(&Refusal::NotASplit));
        assert!(!refusal_retires(&Refusal::EmptyReplacement));
        assert!(!refusal_retires(&Refusal::NotPending));
        assert!(!refusal_retires(&Refusal::UnknownKind("x".into())));
        assert!(refusal_retires(&Refusal::AllSourcesGone));
    }

    #[test]
    fn an_already_resolved_proposal_is_not_applied_again() {
        let mut p = proposal("capture", Some("x"), None);
        p.status = "approved".into();
        assert_eq!(plan_apply(&p, all_alive), Err(Refusal::NotPending));
        p.status = "rejected".into();
        assert_eq!(plan_apply(&p, all_alive), Err(Refusal::NotPending));
    }

    #[test]
    fn an_unknown_kind_is_refused_by_name() {
        assert_eq!(
            plan_apply(&proposal("curate", Some("x"), None), all_alive),
            Err(Refusal::UnknownKind("curate".into()))
        );
    }

    #[test]
    fn a_capture_with_no_targets_never_consults_the_store() {
        // The resurrection guard applies only to proposals that NAME sources. A capture
        // that touched it would be blocked by an unrelated missing fact.
        // Cell, because plan_apply takes an `Fn` — the guard must be callable more than
        // once per proposal, since a tidy names several sources.
        let consulted = std::cell::Cell::new(false);
        let steps = plan_apply(&proposal("capture", Some("x"), None), |_| {
            consulted.set(true);
            false
        })
        .unwrap();
        assert_eq!(steps.len(), 1);
        assert!(!consulted.get());
    }

    #[test]
    fn a_resolved_contradiction_stores_the_new_fact_and_retires_the_refuted_one() {
        // The whole point: after approval, recall must stop returning what was
        // disproved. Anything less leaves the refuted fact ranked above its own
        // correction, which is what started this.
        let steps = plan_apply(&proposal("contradiction", Some("the correct thing"),
                                         Some("old-id")), all_alive)
            .expect("a contradiction is appliable");
        assert_eq!(steps, vec![Step::StoreAndRetire {
            dataset: "ds".into(),
            text: "the correct thing".into(),
            old_ids: vec!["old-id".into()],
        }]);
    }

    #[test]
    fn a_contradiction_with_nothing_to_replace_is_refused_not_silently_stored() {
        assert_eq!(plan_apply(&proposal("contradiction", Some("   "), Some("old-id")),
                              all_alive),
                   Err(Refusal::EmptyReplacement));
    }

    #[test]
    fn whitespace_around_ids_and_parts_is_tolerated() {
        let text = format!(" one {SPLIT_SEP} two ");
        let steps = plan_apply(&proposal("split", Some(&text), Some(" a , b ")), all_alive)
            .unwrap();
        assert_eq!(steps[0], Step::Store { dataset: "ds".into(), text: "one".into() });
        match &steps[1] {
            Step::StoreAndRetire { text, old_ids, .. } => {
                assert_eq!(text, "two");
                assert_eq!(old_ids, &vec!["a".to_string(), "b".to_string()]);
            }
            other => panic!("expected a retiring step, got {other:?}"),
        }
    }
}
