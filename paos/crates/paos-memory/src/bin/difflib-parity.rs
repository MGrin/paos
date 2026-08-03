//! Print `difflib::ratio` for each pair on stdin, one ratio per line.
//!
//! Input is two escaped lines per pair (`\\` -> `\`, `\n` -> newline), which keeps this
//! binary free of a JSON dependency — paos-memory ships inside paosd, and a parity
//! harness is not a reason to grow it.
//!
//! The counterpart is `paos/parity/difflib.py ratios`, which prints CPython's
//! `difflib.SequenceMatcher(None, a, b).ratio()` for the SAME file, so any difference is
//! the algorithm and not the sampling.

use std::io::Read;

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn main() {
    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        eprintln!("difflib-parity: cannot read stdin");
        std::process::exit(1);
    }
    let lines: Vec<&str> = raw.lines().collect();
    if lines.len() % 2 != 0 {
        eprintln!("difflib-parity: expected an even number of lines (two per pair)");
        std::process::exit(1);
    }
    let mut out = String::new();
    for pair in lines.chunks(2) {
        let a = unescape(pair[0]);
        let b = unescape(pair[1]);
        // 17 significant digits round-trips an f64, so the comparison is on the number
        // rather than on a rounding of it.
        out.push_str(&format!("{:.17}\n", paos_memory::difflib::ratio(&a, &b)));
    }
    print!("{out}");
}
