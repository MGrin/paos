//! Advisory screening of review-queue proposals.
//!
//! **Screening never rejects anything.** It sorts. Flagged proposals go last with the
//! matched phrase quoted, so the queue becomes a triage instead of a linear read that
//! gets abandoned partway.
//!
//! That it stays advisory is a measured decision, not caution. A focused judge run over
//! the 40 reviewed captures caught 79% of the human's rejections — but dropped 3 of 6
//! facts the human APPROVED, and on reading the disagreements the model was more
//! consistent with the stated policy than the human was. It kept "piping masks a
//! command's exit code" (a real gotcha the human rejected) and dropped "the fix is pushed
//! at SHA 0804b164" (approved, though the policy calls that status). So the labels are
//! not ground truth in either direction, there is nothing trustworthy to calibrate
//! against, and an automatic filter would silently discard good facts.
//!
//! The deterministic rules below were checked against all 40: they flag 5 of 34
//! rejections and 0 of 6 approvals. Modest, and honest about being modest.

use regex::Regex;
use std::sync::OnceLock;

/// (label, pattern). Order matters: the FIRST rule that matches wins, exactly as
/// Python's sequential `re.search` over the same list does.
const RULES: &[(&str, &str)] = &[
    (
        "task status",
        r"\btask \d+\b|\bis (?:complete|stable|done)\b|\bare complete\b|\bpassing all\b|\ball (?:unit )?tests? pass\b|\btests? (?:pass|passed|passing)\b",
    ),
    (
        "version number",
        r"\bversion (?:has been |was )?(?:updated|bumped|set) to\b|\bversion to \d",
    ),
    (
        "code structure",
        r"\bincludes facets\b|\bkey features include\b|\bthe \w+ subparser\b|\bproject (?:includes|contains|has)\b",
    ),
];

fn compiled() -> &'static Vec<(&'static str, Regex)> {
    static C: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    C.get_or_init(|| {
        RULES
            .iter()
            .map(|(name, pat)| {
                (*name, Regex::new(pat).expect("screening rule must compile"))
            })
            .collect()
    })
}

/// `(flag, why)` for a candidate fact, or `None` if nothing obvious fires.
///
/// The `why` quotes the matched text, so the span has to agree with Python's `re`, not
/// just the boolean.
pub fn screen_proposal(text: &str) -> Option<(String, String)> {
    // Python lowercases the WHOLE text first and matches against that, so the quoted
    // match is lowercase too. Matching case-insensitively against the original would
    // quote the original casing and diverge.
    let low = text.to_lowercase();
    for (name, re) in compiled() {
        if let Some(m) = re.find(&low) {
            // Python formats with %r, which for a str with no quotes or backslashes is
            // 'single-quoted'. Every real match here is plain lowercase words.
            return Some(("noise".to_string(), format!("{name} — matched {}", py_repr(m.as_str()))));
        }
    }
    None
}

/// Python's `%r` for a plain string: single quotes unless the value contains one.
fn py_repr(s: &str) -> String {
    if s.contains('\'') && !s.contains('"') {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

/// Whether a proposal kind is screened at all.
///
/// Only `capture` and `lesson`. A split or tidy carries merged text whose phrasing comes
/// from facts the operator already accepted, so scoring it against capture rules would
/// flag work the human has effectively approved once already.
pub fn is_screened_kind(kind: &str) -> bool {
    matches!(kind, "capture" | "lesson")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flag(t: &str) -> Option<String> {
        screen_proposal(t).map(|(_, why)| why)
    }

    #[test]
    fn task_status_is_flagged() {
        assert!(flag("task 12 is complete").is_some());
        assert!(flag("all tests pass on main").is_some());
        assert!(flag("all unit tests pass").is_some());
    }

    #[test]
    fn a_version_bump_is_flagged() {
        assert!(flag("version has been bumped to 1.2.3").is_some());
        assert!(flag("version to 4").is_some());
    }

    #[test]
    fn code_structure_is_flagged() {
        assert!(flag("the project includes facets for bus and memory").is_some());
        assert!(flag("the memory subparser takes a scope").is_some());
    }

    #[test]
    fn a_real_gotcha_is_not_flagged() {
        assert_eq!(
            flag("piping masks a command's exit code: cmd | tail returns tail's exit"),
            None,
            "this exact fact was rejected by a human and is still a real gotcha"
        );
    }

    #[test]
    fn an_external_system_quirk_is_not_flagged() {
        assert_eq!(flag("Xero's export button needs a 30s timeout"), None);
    }

    #[test]
    fn the_reason_quotes_the_text_that_triggered_it() {
        let why = flag("everything is stable now").expect("should flag");
        assert!(why.contains("task status"), "names the rule: {why}");
        assert!(why.contains("'is stable'"), "quotes the match: {why}");
    }

    #[test]
    fn the_quoted_match_is_lowercased_like_pythons() {
        // Python lowercases the text before searching, so the match it quotes is lower
        // case even when the input was not.
        let why = flag("ALL TESTS PASS").expect("should flag");
        assert!(why.contains("'all tests pass'"), "{why}");
    }

    #[test]
    fn only_captures_and_lessons_are_screened() {
        assert!(is_screened_kind("capture"));
        assert!(is_screened_kind("lesson"));
        for k in ["split", "tidy", "supersede"] {
            assert!(!is_screened_kind(k), "{k} carries already-approved phrasing");
        }
    }

    #[test]
    fn rule_order_beats_match_position() {
        // "project includes" (rule 3) sits at offset 0 and "task 5" (rule 1) sits at the
        // end, yet Python's sequential re.search over the rule list returns rule 1.
        // Verified against librarian_facet.screen_proposal, not assumed.
        let why = flag("project includes many things and task 5").expect("should flag");
        assert!(why.contains("task status"), "rule order decides, not offset: {why}");
        assert!(why.contains("'task 5'"), "{why}");

        // And the converse: when rule 1 genuinely does not match, rule 3 wins. This is
        // the case that caught my first, wrong version of this test — "tests that pass"
        // does NOT match `\btests? (?:pass|...)\b`, which needs them adjacent.
        let why = flag("the project includes tests that pass").expect("should flag");
        assert!(why.contains("code structure"), "{why}");
        assert!(why.contains("'project includes'"), "{why}");
    }
}
