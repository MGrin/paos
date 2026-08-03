//! An exact port of CPython's `difflib.SequenceMatcher(None, a, b).ratio()`.
//!
//! This is NOT the same notion of "near-duplicate" as `health.rs`, and the two must not
//! be confused: health compares EMBEDDINGS by cosine at 0.95, this compares CHARACTERS by
//! Ratcliff-Obershelp at 0.82 (`COG_SUPERSEDE_THRESHOLD`). Different algorithm, different
//! scale, different threshold.
//!
//! Why it has to be exact rather than merely close: `librarian.draft()` BRANCHES on this
//! number. A fact that scores at or above the threshold is queued as a `supersede`
//! proposal — replacing an existing fact — instead of a `capture`. So the ratio decides
//! what a human is asked to approve, and drift changes the proposal rather than a
//! warning string.
//!
//! The subtle part is `autojunk`, which is ON by default in Python and easy to miss.
//! Once `b` reaches 200 characters, any character occurring more than `len(b)/100 + 1`
//! times is treated as "popular" and removed from the index, so it can no longer START a
//! match. Most stored facts are over 200 characters, so this is the common path, not an
//! edge case. Omitting it produces plausible-looking ratios that are simply different.

use std::collections::HashMap;

/// Character-level Ratcliff-Obershelp similarity, matching
/// `difflib.SequenceMatcher(None, a, b).ratio()` including `autojunk=True`.
pub fn ratio(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let total = a.len() + b.len();
    if total == 0 {
        // Python's _calculate_ratio returns 1.0 for two empty sequences.
        return 1.0;
    }
    let m = Matcher::new(&a, &b);
    let matches = m.total_matching();
    2.0 * matches as f64 / total as f64
}

struct Matcher<'a> {
    a: &'a [char],
    b: &'a [char],
    /// Character -> ascending indices in `b`. Popular characters are absent (autojunk).
    b2j: HashMap<char, Vec<usize>>,
}

impl<'a> Matcher<'a> {
    fn new(a: &'a [char], b: &'a [char]) -> Self {
        let mut b2j: HashMap<char, Vec<usize>> = HashMap::new();
        for (i, ch) in b.iter().enumerate() {
            b2j.entry(*ch).or_default().push(i);
        }
        // autojunk: only once b is long enough for the heuristic to be worth anything.
        let n = b.len();
        if n >= 200 {
            let ntest = n / 100 + 1;
            b2j.retain(|_, idxs| idxs.len() <= ntest);
        }
        Matcher { a, b, b2j }
    }

    /// Sum of the sizes of every matching block.
    ///
    /// Python's `get_matching_blocks` also merges adjacent blocks and appends a
    /// zero-length terminator. Both leave the SUM unchanged — merging adds the two sizes
    /// and the terminator is 0 — and `ratio()` only ever uses the sum, so neither step is
    /// reproduced here.
    fn total_matching(&self) -> usize {
        let mut queue = vec![(0usize, self.a.len(), 0usize, self.b.len())];
        let mut total = 0usize;
        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let (i, j, k) = self.find_longest_match(alo, ahi, blo, bhi);
            if k > 0 {
                total += k;
                if alo < i && blo < j {
                    queue.push((alo, i, blo, j));
                }
                if i + k < ahi && j + k < bhi {
                    queue.push((i + k, ahi, j + k, bhi));
                }
            }
        }
        total
    }

    fn find_longest_match(
        &self,
        alo: usize,
        ahi: usize,
        blo: usize,
        bhi: usize,
    ) -> (usize, usize, usize) {
        let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0usize);
        // j2len[j] = length of the longest match ENDING at a[i-1], b[j-1].
        let mut j2len: HashMap<usize, usize> = HashMap::new();
        for i in alo..ahi {
            let mut newj2len: HashMap<usize, usize> = HashMap::new();
            if let Some(idxs) = self.b2j.get(&self.a[i]) {
                for &j in idxs {
                    if j < blo {
                        continue;
                    }
                    // The index list is ascending, so the first out-of-range j ends it.
                    if j >= bhi {
                        break;
                    }
                    let k = j.checked_sub(1).and_then(|jm| j2len.get(&jm).copied()).unwrap_or(0) + 1;
                    newj2len.insert(j, k);
                    if k > bestsize {
                        besti = i + 1 - k;
                        bestj = j + 1 - k;
                        bestsize = k;
                    }
                }
            }
            j2len = newj2len;
        }

        // Extend the match over characters that are equal but were never indexed —
        // i.e. the popular ones autojunk removed. With isjunk=None the junk-only
        // extension loops in CPython can never fire, so they are not reproduced.
        while besti > alo
            && bestj > blo
            && self.a[besti - 1] == self.b[bestj - 1]
        {
            besti -= 1;
            bestj -= 1;
            bestsize += 1;
        }
        while besti + bestsize < ahi
            && bestj + bestsize < bhi
            && self.a[besti + bestsize] == self.b[bestj + bestsize]
        {
            bestsize += 1;
        }
        (besti, bestj, bestsize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    // Expected values below come from running CPython's difflib, not from intuition.

    #[test]
    fn matches_cpython_on_short_strings() {
        assert!(close(ratio("abcd", "bcde"), 0.75));
        assert!(close(ratio("", ""), 1.0));
        assert!(close(ratio("abc", ""), 0.0));
        assert!(close(ratio("abc", "abc"), 1.0));
        assert!(close(ratio("private", "prat"), 0.7272727272727273));
    }

    #[test]
    fn identical_long_text_is_one() {
        let s = "the quick brown fox ".repeat(40);
        assert!(close(ratio(&s, &s), 1.0), "autojunk must not break self-similarity");
    }

    #[test]
    fn counts_characters_not_bytes() {
        // A multi-byte string compared with itself is still a perfect match, and the
        // denominator is the CHARACTER count.
        assert!(close(ratio("日本語", "日本語"), 1.0));
        assert!(close(ratio("日本語", "日本"), 0.8));
    }

    #[test]
    fn autojunk_engages_past_two_hundred_characters() {
        // Under 200 the index keeps every character; at/over 200 the popular ones go.
        let b_short: Vec<char> = "ab".repeat(99).chars().collect(); // 198
        let b_long: Vec<char> = "ab".repeat(150).chars().collect(); // 300
        let a: Vec<char> = "ab".chars().collect();
        assert_eq!(Matcher::new(&a, &b_short).b2j.len(), 2, "no autojunk under 200");
        assert!(
            Matcher::new(&a, &b_long).b2j.is_empty(),
            "'a' and 'b' each occur 150 times in 300 chars, far over the len/100+1 floor"
        );
    }

    #[test]
    fn a_realistic_near_duplicate_pair_scores_over_the_threshold() {
        let a = "paos memory writes ALL go through paosd, never Python-side SQLite, \
                 because the daemon is the single writer and owns the embedding model.";
        let b = "paos memory writes all go through paosd and never Python-side SQLite, \
                 since the daemon is the single writer and owns the embedding model.";
        let r = ratio(a, b);
        assert!(r >= 0.82, "expected a near-duplicate, got {r}");
    }

    #[test]
    fn unrelated_facts_score_below_the_threshold() {
        let a = "Port 8000 on this Mac is free as of 2026-07-30.";
        let b = "The nightly dream is opt-in and off by default.";
        assert!(ratio(a, b) < 0.82);
    }
}
