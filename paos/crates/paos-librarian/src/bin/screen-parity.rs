//! Print `screen_proposal` for each escape-encoded line on stdin.
//!
//! Counterpart to `paos/parity/screen_parity.py screen`. Same file in, same format out,
//! so a difference is the rules and not the sampling.

use std::io::Read;

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(o) => out.push(o),
            None => out.push('\\'),
        }
    }
    out
}

fn main() {
    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        eprintln!("screen-parity: cannot read stdin");
        std::process::exit(1);
    }
    let mut out = String::new();
    for line in raw.lines() {
        match paos_librarian::screen_proposal(&unescape(line)) {
            Some((flag, why)) => out.push_str(&format!("{flag}\t{why}\n")),
            None => out.push_str("-\n"),
        }
    }
    print!("{out}");
}
