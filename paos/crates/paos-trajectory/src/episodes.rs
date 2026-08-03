//! Failure-episode extraction.
//!
//! 953 sessions were archived on this machine in ten days, each rediscovering the same
//! traps independently, because the only durable record of a trap was prose a human
//! typed by hand. Their own meta-observation: "in almost every incident the human was
//! the detector."
//!
//! A failure episode is the machine-readable version: what was tried, how it failed, and
//! what happened next. Extraction is deliberately dumb — no LLM, no judgement — so it can
//! run over every transcript on the machine cheaply and let the distiller decide which
//! episodes deserve a lesson.

use crate::{render_text, Record};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Episode {
    pub tool: String,
    pub args: String,
    pub error: String,
    pub signature: String,
    pub timestamp: Option<String>,
    pub recovery: String,
}

/// After noise-stripping a signature must still say something. `Bash|exit code <n>`
/// groups every non-zero exit on the machine into one bucket — it recurs constantly and
/// teaches nothing, so it would win the ranking and crowd out real patterns.
const MIN_SIGNAL_CHARS: usize = 24;

const SIGNATURE_WIDTH: usize = 120;

/// Strip the tokens that make an error unique to one run but say nothing about the
/// failure MODE, so "no such file: /Users/a/x-1785472925.json" and the same error
/// tomorrow collapse onto one signature.
///
/// Hand-rolled rather than pulling in `regex`, and exactly equivalent to the three
/// Python patterns. The equivalence rests on one observation: `\b[0-9a-f]{7,}\b` and
/// `\b\d+\b` both match only characters that are themselves word characters, so a match
/// can only ever be a COMPLETE maximal word-run. That turns two backtracking regexes
/// into a single scan with no ambiguity.
fn strip_noise(input: &str) -> String {
    // 1. `/[^\s'"]+` -> <path>. Note the `+`: a lone slash followed by whitespace is
    //    NOT a path and is left alone.
    let mut after_paths = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '/' {
            let mut j = i + 1;
            while j < chars.len()
                && !chars[j].is_whitespace()
                && chars[j] != '\''
                && chars[j] != '"'
            {
                j += 1;
            }
            if j > i + 1 {
                after_paths.push_str("<path>");
                i = j;
                continue;
            }
        }
        after_paths.push(chars[i]);
        i += 1;
    }

    // 2. `\b[0-9a-f]{7,}\b` -> <hash>, then 3. `\b\d+\b` -> <n>. Both are decided per
    //    word-run. Hash wins on an all-digit run of 7+ because Python applies it first.
    let mut after_words = String::with_capacity(after_paths.len());
    let wchars: Vec<char> = after_paths.chars().collect();
    let mut i = 0;
    while i < wchars.len() {
        if is_word(wchars[i]) {
            let start = i;
            while i < wchars.len() && is_word(wchars[i]) {
                i += 1;
            }
            let run = &wchars[start..i];
            if run.len() >= 7 && run.iter().all(|c| c.is_ascii_hexdigit()) {
                after_words.push_str("<hash>");
            } else if run.iter().all(|c| c.is_ascii_digit()) {
                after_words.push_str("<n>");
            } else {
                after_words.extend(run.iter());
            }
            continue;
        }
        after_words.push(wchars[i]);
        i += 1;
    }

    // 4. `\s+` -> " "
    let mut out = String::with_capacity(after_words.len());
    let mut in_space = false;
    for c in after_words.chars() {
        if c.is_whitespace() {
            if !in_space {
                out.push(' ');
                in_space = true;
            }
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

/// A stable key for "this kind of failure", for counting recurrence across sessions.
///
/// Recurrence is the whole gate on lessons: the `capture` proposal kind already runs an
/// 85% rejection rate, so proposing from a single occurrence would just add noise at
/// volume. A trap that bit two different sessions is evidence; one that bit once is an
/// anecdote.
pub fn error_signature(tool_name: Option<&str>, error_text: &str) -> String {
    let stripped = strip_noise(error_text.trim());
    let capped: String = stripped.chars().take(SIGNATURE_WIDTH).collect();
    format!("{}|{}", tool_name.unwrap_or("?"), capped.trim().to_lowercase())
}

/// Whether a signature carries enough detail to be worth a lesson.
pub fn is_teachable(signature: &str) -> bool {
    let body = match signature.split_once('|') {
        Some((_, b)) => b,
        None => signature,
    };
    let stripped = body.replace("<path>", "").replace("<hash>", "").replace("<n>", "");
    stripped.trim().chars().count() >= MIN_SIGNAL_CHARS
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Every failed tool result, with what was attempted and what followed.
///
/// `context` records of aftermath, because the FIX is what makes an episode worth
/// keeping and it is never in the error itself. Stops early at the next failure so two
/// adjacent errors do not each claim the other's recovery.
pub fn failure_episodes(records: &[Record], context: usize) -> Vec<Episode> {
    let mut by_id: std::collections::HashMap<&str, &crate::ToolCall> =
        std::collections::HashMap::new();
    for r in records {
        if let Record::Assistant(a) = r {
            for tc in a.tool_calls.as_deref().unwrap_or(&[]) {
                if let Some(id) = tc.id.as_deref() {
                    if !id.is_empty() {
                        by_id.insert(id, tc);
                    }
                }
            }
        }
    }

    let mut episodes = Vec::new();
    for (i, r) in records.iter().enumerate() {
        let t = match r {
            Record::Tool(t) if t.is_error == Some(true) => t,
            _ => continue,
        };
        let call = t.tool_call_id.as_deref().and_then(|id| by_id.get(id).copied());
        let mut after: Vec<Record> = Vec::new();
        for nxt in records.iter().skip(i + 1).take(context) {
            if let Record::Tool(nt) = nxt {
                if nt.is_error == Some(true) {
                    break;
                }
            }
            after.push(nxt.clone());
        }
        let error = t.content.trim().to_string();
        let name = call.and_then(|c| c.name.as_deref());
        episodes.push(Episode {
            tool: name.unwrap_or("?").to_string(),
            args: call.map(|c| c.args.clone()).unwrap_or_default(),
            error: error.clone(),
            signature: error_signature(name, &error),
            timestamp: t.timestamp.clone(),
            recovery: render_text(&after, 600),
        });
    }
    episodes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Assistant, TextRecord, Tool, ToolCall};

    fn tool_err(id: &str, content: &str) -> Record {
        Record::Tool(Tool {
            role: "tool",
            tool_call_id: Some(id.into()),
            content: content.into(),
            timestamp: None,
            is_error: Some(true),
        })
    }

    fn call(id: &str, name: &str) -> Record {
        Record::Assistant(Assistant {
            role: "assistant",
            content: None,
            timestamp: None,
            tool_calls: Some(vec![ToolCall {
                id: Some(id.into()),
                name: Some(name.into()),
                args: "{}".into(),
            }]),
        })
    }

    #[test]
    fn the_signature_ignores_run_specific_noise() {
        let a = error_signature(Some("Bash"), "no such file: /Users/a/x-1785472925.json");
        let b = error_signature(Some("Bash"), "no such file: /Users/b/y-9999999999.json");
        assert_eq!(a, b, "two runs of the same trap must collapse to one signature");
        assert!(a.contains("<path>"));
    }

    #[test]
    fn the_signature_still_separates_genuinely_different_failures() {
        assert_ne!(
            error_signature(Some("Bash"), "permission denied"),
            error_signature(Some("Bash"), "connection refused")
        );
    }

    #[test]
    fn the_signature_separates_the_same_error_from_different_tools() {
        assert_ne!(
            error_signature(Some("Bash"), "boom"),
            error_signature(Some("Read"), "boom")
        );
    }

    #[test]
    fn a_hash_run_collapses_but_a_short_hex_word_does_not() {
        assert_eq!(strip_noise("sha deadbeef1 here"), "sha <hash> here");
        // 6 hex chars is under the 7-char floor and is not a digit run, so it survives.
        assert_eq!(strip_noise("sha deadbe here"), "sha deadbe here");
    }

    #[test]
    fn a_word_run_that_is_only_partly_hex_is_left_alone() {
        // `\b[0-9a-f]{7,}\b` cannot match inside a longer word: there is no word
        // boundary mid-run. This is the case a naive substring scan gets wrong.
        assert_eq!(strip_noise("id deadbeefzz end"), "id deadbeefzz end");
        assert_eq!(strip_noise("v1234567x"), "v1234567x");
    }

    #[test]
    fn a_seven_digit_run_becomes_a_hash_not_a_number() {
        // Python applies the hash pattern first, and 1234567 satisfies both.
        assert_eq!(strip_noise("n 1234567"), "n <hash>");
        assert_eq!(strip_noise("n 123456"), "n <n>");
    }

    #[test]
    fn a_lone_slash_is_not_a_path() {
        // The pattern is `/[^\s'"]+` — one or more chars AFTER the slash.
        assert_eq!(strip_noise("a / b"), "a / b");
        assert_eq!(strip_noise("a /usr/bin/x b"), "a <path> b");
    }

    #[test]
    fn whitespace_collapses() {
        assert_eq!(strip_noise("a \n\t  b"), "a b");
    }

    #[test]
    fn a_signature_with_no_substance_is_not_teachable() {
        assert!(!is_teachable("Bash|exit code <n>"));
        assert!(!is_teachable("Bash|<path> <hash>"));
    }

    #[test]
    fn a_signature_naming_a_real_trap_is_teachable() {
        assert!(is_teachable(
            "Bash|refusing to run: backticks in a paos message body are stripped"
        ));
    }

    #[test]
    fn an_episode_carries_what_was_tried_and_what_followed() {
        let recs = vec![
            call("t1", "Bash"),
            tool_err("t1", "boom"),
            Record::Text(TextRecord {
                role: "user",
                content: "try again".into(),
                timestamp: None,
            }),
        ];
        let eps = failure_episodes(&recs, 6);
        assert_eq!(eps.len(), 1);
        assert_eq!(eps[0].tool, "Bash");
        assert_eq!(eps[0].error, "boom");
        assert!(eps[0].recovery.contains("try again"), "the fix is never in the error");
    }

    #[test]
    fn a_clean_session_yields_no_episodes() {
        let recs = vec![
            call("t1", "Bash"),
            Record::Tool(Tool {
                role: "tool",
                tool_call_id: Some("t1".into()),
                content: "fine".into(),
                timestamp: None,
                is_error: None,
            }),
        ];
        assert!(failure_episodes(&recs, 6).is_empty());
    }

    #[test]
    fn adjacent_failures_do_not_claim_each_others_recovery() {
        let recs = vec![
            call("t1", "Bash"),
            tool_err("t1", "first"),
            tool_err("t2", "second"),
            Record::Text(TextRecord {
                role: "user",
                content: "the actual fix".into(),
                timestamp: None,
            }),
        ];
        let eps = failure_episodes(&recs, 6);
        assert_eq!(eps.len(), 2);
        assert_eq!(eps[0].recovery, "", "stops at the next failure");
        assert!(eps[1].recovery.contains("the actual fix"));
    }

    #[test]
    fn an_episode_for_an_unmatched_call_id_still_records_the_error() {
        let eps = failure_episodes(&[tool_err("missing", "boom")], 6);
        assert_eq!(eps.len(), 1);
        assert_eq!(eps[0].tool, "?");
        assert_eq!(eps[0].args, "");
        assert!(eps[0].signature.starts_with("?|"));
    }
}
