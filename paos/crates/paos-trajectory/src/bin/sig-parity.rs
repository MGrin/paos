//! Sign each `tool<TAB>error` pair on stdin, one signature per line.
//!
//! Isolates the noise-stripper from the rest of the funnel: if the funnel disagrees but
//! this does not, the difference is upstream of signing.

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
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some(o) => out.push(o),
            None => out.push('\\'),
        }
    }
    out
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n").replace('\r', "\\r").replace('\t', "\\t")
}

fn main() {
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let mut out = String::new();
    for line in raw.lines() {
        let (tool, err) = line.split_once('\t').unwrap_or((line, ""));
        let sig = paos_trajectory::error_signature(
            Some(&unescape(tool)),
            &unescape(err),
        );
        out.push_str(&escape(&sig));
        out.push('\n');
    }
    print!("{out}");
}
