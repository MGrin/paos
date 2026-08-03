//! `dream` — mine recent sessions into candidate memories.
//!
//! The scheduler already lives in `paosd`; this is the pass itself.

/// Chars per chunk on the local backend.
pub const CHUNK_CHARS_LOCAL: usize = 12_000;
/// Chars per chunk on the Claude backend, which handles a far larger prompt.
pub const CHUNK_CHARS_CLAUDE: usize = 400_000;
/// At most this many chunks per session.
pub const MAX_CHUNKS: usize = 6;
/// Tool output is truncated hard for dream: the point is the shape of the session, not
/// the contents of every file it read.
pub const TOOL_TRUNCATE: usize = 160;

pub fn chunk_chars(backend: &str) -> usize {
    if backend == "claude" {
        CHUNK_CHARS_CLAUDE
    } else {
        CHUNK_CHARS_LOCAL
    }
}

/// Split text into <=`size`-char chunks on LINE boundaries, never mid-line.
///
/// Splits on `\n` and ONLY `\n`. `str.splitlines()` also breaks on U+2028, U+2029, VT,
/// FF, FS, GS, RS and NEL, so a chunker built on it rejoins those as newlines and
/// silently REWRITES text it was only supposed to be cutting — and that rewritten text is
/// what reaches the distiller, so a fact containing one is altered before the model ever
/// sees it. Measured when this was found: 5 of 121 real sessions carried one.
///
/// This port never reproduced that; the Python was fixed to match in e5cfd4f6, and the
/// parity corpus no longer excludes anything. The tests below are what keep both sides
/// honest — they assert the separators SURVIVE, which is the property, not the history.
pub fn chunk_lines(text: &str, size: usize) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let mut n = 0usize;
    for line in text.split('\n') {
        // Python measures len(line) + 1 for the newline it will rejoin with, and counts
        // CHARACTERS.
        let len = line.chars().count();
        if n > 0 && n + len + 1 > size {
            chunks.push(cur.join("\n"));
            cur.clear();
            n = 0;
        }
        cur.push(line);
        n += len + 1;
    }
    if !cur.is_empty() {
        chunks.push(cur.join("\n"));
    }
    chunks
}

/// Per-session outcome of a dream pass.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionResult {
    pub path: String,
    pub session: Option<String>,
    /// Chunks actually used (after the cap).
    pub chunks: usize,
    pub chunks_total: usize,
    pub capped: bool,
    pub chars: usize,
    /// Chunks the distiller returned nothing for.
    ///
    /// Counted so "zero proposals" can distinguish "every chunk came back empty — check
    /// the backend" from "nothing durable happened". Those look identical otherwise, and
    /// the librarian being quietly broken went unnoticed for weeks for exactly that
    /// reason.
    pub silent_chunks: usize,
    pub proposals: Vec<i64>,
    pub error: Option<String>,
    pub skipped: Option<&'static str>,
}

/// Which datasets still need housekeeping this run.
///
/// Housekeep each dataset ONCE per run. Several sessions commonly share a repo, and
/// re-running tidy/split per session queued the same merge two or three times —
/// duplicate proposals are how a review queue becomes ignorable.
#[derive(Debug, Default)]
pub struct Housekept(std::collections::HashSet<String>);

impl Housekept {
    pub fn new() -> Self {
        Self::default()
    }

    /// True the FIRST time a dataset is seen, false afterwards.
    pub fn claim(&mut self, dataset: &str) -> bool {
        self.0.insert(dataset.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_break_on_line_boundaries_only() {
        let text = "aaaa\nbbbb\ncccc\ndddd";
        let c = chunk_lines(text, 10);
        assert!(c.iter().all(|ch| !ch.starts_with('\n') && !ch.ends_with('\n')));
        assert_eq!(c.join("\n"), text, "chunking must not lose or add content");
    }

    #[test]
    fn a_short_text_is_one_chunk() {
        assert_eq!(chunk_lines("one\ntwo", 1000), vec!["one\ntwo"]);
        assert_eq!(chunk_lines("", 1000), vec![""]);
    }

    #[test]
    fn a_single_over_long_line_is_not_split() {
        // No mid-line cuts, even when the line alone exceeds the budget. Cutting mid-line
        // would hand the model half a tool invocation.
        let long = "x".repeat(500);
        assert_eq!(chunk_lines(&long, 10), vec![long]);
    }

    #[test]
    fn u2028_is_preserved_rather_than_rewritten_to_a_newline() {
        // The property, not the history: chunking must not alter the text it cuts. Both
        // implementations used to disagree here (splitlines() rewrote U+2028 to a
        // newline); the Python was fixed in e5cfd4f6 and this is what keeps it fixed.
        let text = "alpha\u{2028}beta";
        let c = chunk_lines(text, 1000);
        assert_eq!(c, vec![text], "must round-trip unchanged");
        assert!(c[0].contains('\u{2028}'), "the separator survives");
        assert!(!c[0].contains('\n'), "and is NOT rewritten to a newline");
    }

    #[test]
    fn every_separator_splitlines_would_eat_survives_chunking() {
        for sep in ['\u{2028}', '\u{2029}', '\u{85}', '\u{b}', '\u{c}', '\u{1c}', '\u{1d}',
                    '\u{1e}'] {
            let text = format!("a{sep}b");
            assert_eq!(chunk_lines(&text, 1000), vec![text.clone()],
                       "U+{:04X} must survive", sep as u32);
        }
    }

    #[test]
    fn chunking_counts_characters_not_bytes() {
        // Verified against librarian_facet._chunk_lines rather than reasoned about: it
        // returns ["日本語\n日本語", "日本語"] for this input. A byte count would see 9
        // characters per line instead of 3 and chunk after every one.
        let text = "日本語\n日本語\n日本語";
        assert_eq!(chunk_lines(text, 8), vec!["日本語\n日本語", "日本語"]);
    }

    #[test]
    fn the_backend_decides_the_chunk_size() {
        assert_eq!(chunk_chars("claude"), CHUNK_CHARS_CLAUDE);
        assert_eq!(chunk_chars("local"), CHUNK_CHARS_LOCAL);
        assert!(CHUNK_CHARS_CLAUDE > CHUNK_CHARS_LOCAL);
    }

    #[test]
    fn housekeeping_runs_once_per_dataset_not_once_per_session() {
        // Several sessions commonly share a repo; re-running tidy per session queued the
        // same merge two or three times.
        let mut h = Housekept::new();
        assert!(h.claim("proj_a"), "first session in this repo does the housekeeping");
        assert!(!h.claim("proj_a"), "the second must not repeat it");
        assert!(h.claim("proj_b"), "a different repo is still its own");
    }
}
