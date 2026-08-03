//! Chunk each escape-encoded text on stdin and print the chunk boundaries.
//!
//! Output is one line per input: `<n-chunks>\t<len,len,len>`. Boundaries rather than
//! bodies, because the bodies are megabytes of transcript and the boundary is what the
//! model actually sees differently.
//!
//! Counterpart to `paos/parity/chunk_parity.py`. The corpus deliberately EXCLUDES text
//! containing the separators `str.splitlines()` eats: Python rewrites those to newlines
//! and Rust does not, which is an intended divergence (see `dream::chunk_lines`) and
//! would otherwise mask a real regression.

use std::io::Read;

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' { out.push(c); continue; }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some(o) => out.push(o),
            None => out.push('\\'),
        }
    }
    out
}

fn main() {
    let size: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(12_000);
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let mut out = String::new();
    // split('\n'), not .lines(): the latter also strips a trailing '\r',
    // which would alter a line that legitimately ends with one.
    for line in raw.split('\n').filter(|l| !l.is_empty()) {
        let chunks = paos_librarian::dream::chunk_lines(&unescape(line), size);
        let lens: Vec<String> =
            chunks.iter().map(|c| c.chars().count().to_string()).collect();
        out.push_str(&format!("{}\t{}\n", chunks.len(), lens.join(",")));
    }
    print!("{out}");
}
