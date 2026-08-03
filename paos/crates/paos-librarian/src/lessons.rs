//! Mining RECURRING failures out of past sessions.
//!
//! 953 sessions were archived on this machine in ten days, each rediscovering the same
//! traps independently. The machine-readable record was on disk the whole time and being
//! thrown away.
//!
//! ## The gate is RECURRENCE, and it does the real work
//!
//! Measured: 547 episodes collapse to 266 signatures, of which 17 appear in two or more
//! sessions. That matters because the `capture` kind already runs an 85% rejection rate —
//! proposing from single occurrences would add noise at exactly the scale that makes a
//! review queue stop being read.
//!
//! DISTINCT SESSIONS, not occurrences. One session stuck in a retry loop would otherwise
//! manufacture its own evidence: the same trap hit forty times in one afternoon is still
//! one session's bad day, while a trap that bit two different sessions is a property of
//! the machine.

use std::collections::HashSet;

/// `LESSON_MIN_SESSIONS` — distinct sessions required before a failure is evidence.
pub const MIN_SESSIONS: usize = 2;
/// `LESSON_MAX` — how many lessons one run may propose.
pub const MAX_LESSONS: usize = 8;

/// One failed tool call and what followed it.
#[derive(Debug, Clone, PartialEq)]
pub struct Episode {
    pub tool: String,
    pub args: String,
    pub error: String,
    pub signature: String,
    pub recovery: String,
}

/// Everything seen for one error signature.
#[derive(Debug, Clone, Default)]
pub struct Group {
    pub episodes: Vec<Episode>,
    /// Session ids, deduplicated — this is what the recurrence gate counts.
    pub sessions: HashSet<String>,
    /// One entry per EPISODE (not per session), matching the Python. `None` means the
    /// session was not in a git repo.
    pub datasets: Vec<Option<String>>,
}

/// Where a lesson belongs, from the evidence rather than from the model.
///
/// One repo keeps the lesson local. The same trap across SEVERAL repos is a property of
/// the machine, and filing it under whichever repo happened to be last would hide it from
/// everywhere else. A single non-repo session in the mix also forces global, because the
/// trap demonstrably bites outside any repo.
pub fn scope_dataset(datasets: &[Option<String>]) -> Option<String> {
    if datasets.is_empty() || datasets.iter().any(|d| d.is_none()) {
        return None;
    }
    let distinct: HashSet<&String> = datasets.iter().flatten().collect();
    if distinct.len() == 1 {
        distinct.into_iter().next().cloned()
    } else {
        None
    }
}

/// The signatures that recur across enough distinct sessions to be worth a lesson,
/// most-recurring first and capped.
///
/// `is_teachable` is applied here, not in the extractor: `Bash|exit code <n>` groups every
/// non-zero exit on the machine into one bucket, so it recurs constantly and would
/// outrank every real pattern while teaching nothing.
pub fn recurring<'a, F>(
    groups: &'a [(String, Group)],
    min_sessions: usize,
    teachable: F,
) -> Vec<(&'a String, &'a Group)>
where
    F: Fn(&str) -> bool,
{
    let mut out: Vec<(&String, &Group)> = groups
        .iter()
        .filter(|(sig, g)| g.sessions.len() >= min_sessions && teachable(sig))
        .map(|(s, g)| (s, g))
        .collect();
    // Most-recurring first: if the cap bites, drop the anecdotes and not the patterns.
    //
    // STABLE, and the input must be in FIRST-ENCOUNTER order. Python sorts a dict, whose
    // iteration order is insertion order, with a stable sort — so ties break on which
    // signature was seen first while walking sessions. Feeding a key-sorted map here
    // instead breaks ties alphabetically, which silently changes WHICH lessons survive
    // the cap when more signatures tie than there are slots.
    out.sort_by(|a, b| b.1.sessions.len().cmp(&a.1.sessions.len()));
    out.truncate(MAX_LESSONS);
    out
}

/// Accumulate groups in FIRST-ENCOUNTER order, which is what the tie-break depends on.
#[derive(Debug, Default)]
pub struct Groups {
    order: Vec<String>,
    by_sig: std::collections::HashMap<String, Group>,
}

impl Groups {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn entry(&mut self, signature: &str) -> &mut Group {
        if !self.by_sig.contains_key(signature) {
            self.order.push(signature.to_string());
            self.by_sig.insert(signature.to_string(), Group::default());
        }
        self.by_sig.get_mut(signature).expect("just inserted")
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Pairs in first-encounter order, ready for `recurring`.
    pub fn ordered(&self) -> Vec<(String, Group)> {
        self.order
            .iter()
            .map(|s| (s.clone(), self.by_sig.get(s).cloned().unwrap_or_default()))
            .collect()
    }
}

/// The user message for one recurring failure.
///
/// At most THREE occurrences. The point is the shared pattern, and ten near-identical
/// stack traces crowd out the recoveries — which are the useful part, because the fix is
/// never in the error itself.
pub fn evidence(group: &Group) -> String {
    let shown: Vec<&Episode> = group.episodes.iter().take(3).collect();
    let body = shown
        .iter()
        .enumerate()
        .map(|(i, e)| {
            format!(
                "--- occurrence {} ---\ntool: {}({})\nfailed with: {}\nwhat happened next:\n{}",
                i + 1,
                e.tool,
                cap(&e.args, 300),
                cap(&e.error, 600),
                cap(
                    if e.recovery.is_empty() { "(nothing recorded)" } else { &e.recovery },
                    1200
                ),
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    format!(
        "This failure hit {} independent sessions. Write the lesson.\n\n{}",
        group.sessions.len(),
        body
    )
}

/// Python slices strings by CODEPOINT. A byte slice would cut differently and can split a
/// character in half.
fn cap(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// The rationale suffix that records how much evidence there was.
pub fn rationale(model_rationale: Option<&str>, sessions: usize) -> String {
    format!(
        "{}{}",
        model_rationale.unwrap_or(""),
        format_args!(" [recurred in {sessions} sessions]")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(sig: &str) -> Episode {
        Episode {
            tool: "Bash".into(),
            args: "{}".into(),
            error: "boom".into(),
            signature: sig.into(),
            recovery: "then it worked".into(),
        }
    }

    fn group(sessions: &[&str], datasets: &[Option<&str>]) -> Group {
        Group {
            episodes: sessions.iter().map(|_| ep("Bash|boom")).collect(),
            sessions: sessions.iter().map(|s| s.to_string()).collect(),
            datasets: datasets.iter().map(|d| d.map(str::to_string)).collect(),
        }
    }

    fn always_teachable(_: &str) -> bool {
        true
    }

    #[test]
    fn a_failure_seen_once_is_an_anecdote_and_is_not_proposed() {
        let mut g = Vec::new();
        g.push(("Bash|boom".to_string(), group(&["s1"], &[Some("p")])));
        assert!(recurring(&g, MIN_SESSIONS, always_teachable).is_empty());
    }

    #[test]
    fn the_same_failure_in_two_sessions_is_evidence() {
        let mut g = Vec::new();
        g.push(("Bash|boom".to_string(), group(&["s1", "s2"], &[Some("p"), Some("p")])));
        assert_eq!(recurring(&g, MIN_SESSIONS, always_teachable).len(), 1);
    }

    #[test]
    fn the_same_failure_twice_in_one_session_is_still_an_anecdote() {
        // THE point of counting distinct sessions: a session in a retry loop must not
        // manufacture its own evidence.
        let mut grp = group(&["s1"], &[Some("p")]);
        grp.episodes.push(ep("Bash|boom"));
        grp.episodes.push(ep("Bash|boom"));
        let mut g = Vec::new();
        g.push(("Bash|boom".to_string(), grp));
        assert!(
            recurring(&g, MIN_SESSIONS, always_teachable).is_empty(),
            "three occurrences, one session — still one session's bad day"
        );
    }

    #[test]
    fn a_contentless_signature_never_recurs_into_a_lesson() {
        // `Bash|exit code <n>` recurs constantly and teaches nothing; letting it through
        // would crowd out every real pattern.
        let mut g = Vec::new();
        g.push(("Bash|exit code <n>".to_string(),
                group(&["s1", "s2", "s3"], &[Some("p"); 3])));
        let teachable = |s: &str| s != "Bash|exit code <n>";
        assert!(recurring(&g, MIN_SESSIONS, teachable).is_empty());
    }

    #[test]
    fn the_most_recurrent_survive_the_cap() {
        let mut g = Vec::new();
        for i in 0..12 {
            let sessions: Vec<String> = (0..=i).map(|j| format!("s{j}")).collect();
            let refs: Vec<&str> = sessions.iter().map(String::as_str).collect();
            let ds = vec![Some("p"); refs.len()];
            g.push((format!("Bash|failure {i:02}"), group(&refs, &ds)));
        }
        let r = recurring(&g, MIN_SESSIONS, always_teachable);
        assert_eq!(r.len(), MAX_LESSONS, "capped");
        assert_eq!(
            r[0].1.sessions.len(),
            12,
            "if the cap bites it must drop anecdotes, not patterns"
        );
        assert!(r.windows(2).all(|w| w[0].1.sessions.len() >= w[1].1.sessions.len()));
    }

    #[test]
    fn ties_break_on_first_encounter_not_alphabetically() {
        // Python sorts a dict (insertion order) with a STABLE sort, so two signatures on
        // the same session count keep the order they were first seen in. Sorting by key
        // instead silently changes WHICH lessons survive the cap when more tie than there
        // are slots — and it is invisible, because the counts are identical either way.
        let mut g = Vec::new();
        g.push(("zzz|seen first".to_string(), group(&["s1", "s2"], &[Some("p"); 2])));
        g.push(("aaa|seen second".to_string(), group(&["s1", "s2"], &[Some("p"); 2])));
        let r = recurring(&g, MIN_SESSIONS, always_teachable);
        assert_eq!(r[0].0, "zzz|seen first", "first encountered wins the tie");
        assert_eq!(r[1].0, "aaa|seen second");
    }

    #[test]
    fn groups_accumulate_in_first_encounter_order() {
        let mut g = Groups::new();
        g.entry("second").sessions.insert("s1".into());
        g.entry("first").sessions.insert("s1".into());
        g.entry("second").sessions.insert("s2".into());
        let o = g.ordered();
        assert_eq!(o.len(), 2);
        assert_eq!(o[0].0, "second", "re-entering must not reorder");
        assert_eq!(o[0].1.sessions.len(), 2);
        assert_eq!(o[1].0, "first");
    }

    #[test]
    fn one_repo_keeps_the_lesson_local() {
        assert_eq!(
            scope_dataset(&[Some("proj_a".into()), Some("proj_a".into())]).as_deref(),
            Some("proj_a")
        );
    }

    #[test]
    fn several_repos_make_it_global() {
        // A trap that bites in two repos is a property of the machine; filing it under
        // whichever was last would hide it from everywhere else.
        assert_eq!(scope_dataset(&[Some("proj_a".into()), Some("proj_b".into())]), None);
    }

    #[test]
    fn a_non_repo_session_in_the_mix_makes_it_global() {
        assert_eq!(scope_dataset(&[Some("proj_a".into()), None]), None);
        assert_eq!(scope_dataset(&[None]), None);
        assert_eq!(scope_dataset(&[]), None);
    }

    #[test]
    fn the_model_is_shown_the_recovery_not_just_the_error() {
        let g = group(&["s1", "s2"], &[Some("p"), Some("p")]);
        let e = evidence(&g);
        assert!(e.contains("This failure hit 2 independent sessions"));
        assert!(e.contains("what happened next:"), "the fix is never in the error");
        assert!(e.contains("then it worked"));
    }

    #[test]
    fn evidence_shows_at_most_three_occurrences() {
        let mut g = group(&["s1", "s2"], &[Some("p"), Some("p")]);
        for _ in 0..10 {
            g.episodes.push(ep("Bash|boom"));
        }
        let e = evidence(&g);
        assert_eq!(e.matches("--- occurrence").count(), 3,
                   "ten near-identical traces crowd out the recoveries");
        assert!(e.contains("occurrence 3"));
        assert!(!e.contains("occurrence 4"));
    }

    #[test]
    fn a_missing_recovery_is_labelled_rather_than_blank() {
        let mut g = group(&["s1", "s2"], &[Some("p"), Some("p")]);
        g.episodes[0].recovery = String::new();
        assert!(evidence(&g).contains("(nothing recorded)"));
    }

    #[test]
    fn evidence_caps_by_characters_not_bytes() {
        let mut g = group(&["s1", "s2"], &[Some("p"), Some("p")]);
        g.episodes[0].error = "日".repeat(1000);
        let e = evidence(&g);
        // 600 CHARACTERS of the error survive; a byte cap would keep 200 and could split
        // a character.
        assert_eq!(e.matches('日').count(), 600);
    }

    #[test]
    fn the_rationale_records_how_much_evidence_there_was() {
        assert_eq!(rationale(Some("a real trap"), 3),
                   "a real trap [recurred in 3 sessions]");
        assert_eq!(rationale(None, 2), " [recurred in 2 sessions]");
    }
}
