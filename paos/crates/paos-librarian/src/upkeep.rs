//! `tidy` and `split` — the two upkeep passes over an existing dataset.
//!
//! Both PROPOSE and never rewrite. Collapsing two facts into one, or cutting one into
//! several, is a judgement call whose failure mode is losing information silently, so
//! everything lands in the human-gated queue.

use crate::draft::parse_candidates;
use std::collections::HashSet;

/// A fact as read from the store.
#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    pub id: String,
    pub text: String,
}

/// Words too common to identify what a fact is ABOUT.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "but", "if", "then", "that", "this", "these", "those",
    "is", "are", "was", "were", "be", "been", "to", "of", "in", "on", "for", "with", "as",
    "at", "by", "it", "its", "from", "not", "no", "you", "your", "we", "our", "i", "my",
    "so", "do", "does", "can", "will", "would", "should", "must", "when", "where",
    "which", "what", "how", "why", "now", "new", "old", "use", "used", "using", "run",
    "runs", "see", "also", "only", "every",
];

/// The Jaccard floor for putting two facts in one group.
///
/// 0.30, and the number is load-bearing. Adding "or >= 3 shared terms" to make a
/// synthetic test pass grouped 47 of 49 facts on the REAL store into 6 sprawling groups —
/// every long fact shares three terms with something. That is the failure that made
/// `curate` propose 44 merges the operator rejected, which taught them to ignore the
/// queue entirely. At 0.30 it finds one tight, genuinely-overlapping group per project.
pub const GROUP_JACCARD: f64 = 0.30;

/// Distinctive terms: `[A-Za-z_][A-Za-z0-9_.\-]{3,}`, lowercased, stopwords removed.
///
/// Hand-rolled rather than a regex because the pattern is a simple first-char/rest
/// classification, and this runs over every fact in a dataset.
pub fn terms(text: &str) -> HashSet<String> {
    let chars: Vec<char> = text.to_lowercase().chars().collect();
    let first_ok = |c: char| c.is_ascii_alphabetic() || c == '_';
    let rest_ok = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-';
    let mut out = HashSet::new();
    let mut i = 0;
    while i < chars.len() {
        if !first_ok(chars[i]) {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        while i < chars.len() && rest_ok(chars[i]) {
            i += 1;
        }
        // {3,} applies to the REST, so the whole token is at least 4 characters.
        if i - start >= 4 {
            let w: String = chars[start..i].iter().collect();
            if !STOPWORDS.contains(&w.as_str()) {
                out.insert(w);
            }
        }
    }
    out
}

/// Group facts plausibly about the same thing — LEXICALLY, not semantically.
///
/// The store has ZERO near-duplicates at cosine 0.95 yet is full of long entries that
/// overlap in content; embedding distance cannot see that, and when `curate` tried it at
/// 0.81-0.84 the operator rejected 44 of 44. So group by shared distinctive terms and let
/// the model read the group and decide. Grouping is a cheap filter to keep prompts small;
/// the judgement is the model's.
pub fn tidy_groups(facts: &[Fact], max_group: usize) -> Vec<Vec<Fact>> {
    let mut remaining: Vec<(Fact, HashSet<String>)> =
        facts.iter().map(|f| (f.clone(), terms(&f.text))).collect();
    let mut groups = Vec::new();
    while !remaining.is_empty() {
        let (seed, seed_terms) = remaining.remove(0);
        let mut group = vec![seed];
        let mut keep = Vec::new();
        for (f, tset) in remaining.into_iter() {
            if seed_terms.is_empty() || tset.is_empty() {
                keep.push((f, tset));
                continue;
            }
            let shared = seed_terms.intersection(&tset).count();
            let union = seed_terms.union(&tset).count();
            let j = shared as f64 / union as f64;
            if j >= GROUP_JACCARD && group.len() < max_group {
                group.push(f);
            } else {
                keep.push((f, tset));
            }
        }
        remaining = keep;
        if group.len() > 1 {
            groups.push(group);
        }
    }
    groups
}

/// The `[id] text` block handed to the tidy prompt.
pub fn numbered(group: &[Fact]) -> String {
    group.iter().map(|f| format!("[{}] {}", f.id, f.text)).collect::<Vec<_>>().join("\n")
}

/// What one group's reply becomes.
#[derive(Debug, Clone, PartialEq)]
pub struct Merge {
    pub text: String,
    pub replaces: Vec<String>,
    pub rationale: String,
}

/// Turn a tidy reply into merges.
pub fn plan_merges(raw: &str) -> Vec<Merge> {
    parse_candidates(raw)
        .into_iter()
        .filter(|c| !c.text.trim().is_empty())
        .map(|c| Merge {
            text: c.text.trim().to_string(),
            replaces: c.replaces.unwrap_or_default(),
            rationale: c
                .why
                .unwrap_or_else(|| "merge/split".to_string())
                .chars()
                .take(300)
                .collect(),
        })
        .collect()
}

/// Why a split proposal was not made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitRefusal {
    /// One part is a REWRITE, not a split, and a rewrite that drops detail is exactly
    /// what this pass must not do.
    NotASplit,
    /// The parts are far shorter than the original, so the model summarised instead of
    /// splitting. Approving that would delete the detail it dropped.
    LostTooMuch,
}

/// The fraction of the original the parts must preserve between them.
pub const SPLIT_KEEP_RATIO: f64 = 0.6;

/// Validate a split reply against the original.
///
/// The two refusals are counted APART from "the model said nothing", because "nothing
/// worth splitting" used to hide three very different outcomes: the model judging an
/// entry coherent, the length guard vetoing its answer, and the model never replying. If
/// the guard is over-refusing, every over-long fact stays bundled forever and the pass
/// still looks like it is working.
pub fn plan_split(raw: &str, original: &str) -> Result<Vec<String>, SplitRefusal> {
    let parts: Vec<String> = parse_candidates(raw)
        .into_iter()
        .map(|c| c.text.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if parts.len() < 2 {
        return Err(SplitRefusal::NotASplit);
    }
    // CHARACTERS, not bytes: Python compares len() of str, and a byte comparison would
    // wrongly accept a split of multi-byte text that dropped most of it.
    let kept: usize = parts.iter().map(|p| p.chars().count()).sum();
    if (kept as f64) < original.chars().count() as f64 * SPLIT_KEEP_RATIO {
        return Err(SplitRefusal::LostTooMuch);
    }
    Ok(parts)
}

/// The rationale a split proposal carries.
pub fn split_rationale(original: &str, parts: usize) -> String {
    format!("unbundle {} chars into {} atomic facts", original.chars().count(), parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(id: &str, text: &str) -> Fact {
        Fact { id: id.into(), text: text.into() }
    }

    #[test]
    fn grouping_finds_shared_distinctive_terms() {
        let facts = vec![
            f("1", "the xero export button needs a thirty second timeout"),
            f("2", "xero export button timeout must be thirty seconds"),
            f("3", "the telegram bridge polls every five seconds"),
        ];
        let g = tidy_groups(&facts, 12);
        assert_eq!(g.len(), 1, "one genuine overlap, not three");
        assert_eq!(g[0].len(), 2);
        assert!(g[0].iter().all(|x| x.id != "3"), "unrelated fact stays out");
    }

    #[test]
    fn stopwords_do_not_create_false_groups() {
        // Nothing in common but filler. Grouping these is how curate produced 44
        // rejected merges.
        let facts = vec![
            f("1", "this is the thing that we should also always use when running"),
            f("2", "that is what you would do with your every, only from here"),
        ];
        assert!(tidy_groups(&facts, 12).is_empty());
    }

    #[test]
    fn a_single_fact_is_never_a_group() {
        assert!(tidy_groups(&[f("1", "alpha bravo charlie delta")], 12).is_empty());
        assert!(tidy_groups(&[], 12).is_empty());
    }

    #[test]
    fn groups_are_bounded() {
        // Ten near-identical facts, max_group 3.
        let facts: Vec<Fact> = (0..10)
            .map(|i| f(&i.to_string(), "xero export button timeout thirty seconds"))
            .collect();
        for g in tidy_groups(&facts, 3) {
            assert!(g.len() <= 3, "a prompt must stay small enough to be read");
        }
    }

    #[test]
    fn terms_needs_four_characters_and_a_letter_first() {
        let t = terms("abc abcd 1234 _abc x.y-z9 THING");
        assert!(!t.contains("abc"), "three characters is below the floor");
        assert!(t.contains("abcd"));
        assert!(!t.contains("1234"), "must start with a letter or underscore");
        assert!(t.contains("_abc"));
        assert!(t.contains("thing"), "lowercased");
    }

    #[test]
    fn terms_keeps_dotted_and_hyphenated_identifiers_whole() {
        // These are the most distinctive tokens in this corpus — splitting them would
        // make every module name look like the word before the dot.
        let t = terms("librarian_facet.py and paos-memory");
        assert!(t.contains("librarian_facet.py"), "{t:?}");
        assert!(t.contains("paos-memory"), "{t:?}");
    }

    #[test]
    fn the_numbered_block_is_what_the_prompt_sees() {
        assert_eq!(numbered(&[f("a", "one"), f("b", "two")]), "[a] one\n[b] two");
    }

    #[test]
    fn a_merge_records_what_it_replaces() {
        let m = plan_merges(r#"[{"text":"merged fact","replaces":["a","b"],"why":"same"}]"#);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].replaces, vec!["a", "b"]);
        assert_eq!(m[0].rationale, "same");
    }

    #[test]
    fn a_merge_without_a_why_still_carries_a_rationale() {
        let m = plan_merges(r#"[{"text":"merged","replaces":["a"]}]"#);
        assert_eq!(m[0].rationale, "merge/split");
    }

    #[test]
    fn one_part_is_a_rewrite_not_a_split_so_it_is_refused() {
        let r = plan_split(r#"[{"text":"a tidier version of the whole thing"}]"#, "orig");
        assert_eq!(r, Err(SplitRefusal::NotASplit));
    }

    #[test]
    fn a_split_that_loses_most_of_the_text_is_refused() {
        let original = "x".repeat(1000);
        let r = plan_split(r#"[{"text":"short one"},{"text":"short two"}]"#, &original);
        assert_eq!(r, Err(SplitRefusal::LostTooMuch), "summarising is not splitting");
    }

    #[test]
    fn a_genuine_split_is_accepted() {
        let original = "a".repeat(50) + &"b".repeat(50);
        let raw = format!(
            r#"[{{"text":"{}"}},{{"text":"{}"}}]"#,
            "a".repeat(50),
            "b".repeat(50)
        );
        assert_eq!(plan_split(&raw, &original).unwrap().len(), 2);
    }

    #[test]
    fn the_keep_ratio_counts_characters_not_bytes() {
        // Multi-byte original: a byte comparison would let a split drop most of it.
        let original = "日".repeat(100);
        let half = "日".repeat(50);
        let raw = format!(r#"[{{"text":"{half}"}},{{"text":"{half}"}}]"#);
        assert!(plan_split(&raw, &original).is_ok(), "100 of 100 characters kept");

        let quarter = "日".repeat(20);
        let raw2 = format!(r#"[{{"text":"{quarter}"}},{{"text":"{quarter}"}}]"#);
        assert_eq!(plan_split(&raw2, &original), Err(SplitRefusal::LostTooMuch));
    }

    #[test]
    fn a_split_rationale_reports_characters() {
        assert_eq!(split_rationale("日本語", 2), "unbundle 3 chars into 2 atomic facts");
    }

    #[test]
    fn an_unreadable_reply_is_not_a_refusal() {
        // "the model said nothing" must stay distinguishable from "I vetoed its answer";
        // both used to collapse into "nothing worth splitting".
        assert_eq!(plan_split("", "orig"), Err(SplitRefusal::NotASplit));
        assert!(plan_merges("").is_empty());
    }
}
