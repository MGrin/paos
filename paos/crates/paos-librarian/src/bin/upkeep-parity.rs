//! Print the tidy groups Rust forms, for a diff against Python's `_tidy_groups`.
//!
//! Counterpart to `paos/parity/upkeep_parity.py groups`. Reads the same
//! `dataset<TAB>id<TAB>escaped-text` file and emits `dataset<TAB>id,id,id` per group,
//! datasets in sorted order — so a difference is the grouping, not the input or the
//! ordering.

use paos_librarian::upkeep::{tidy_groups, Fact};
use std::collections::BTreeMap;
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

fn main() {
    let path = std::env::args().nth(1);
    let mut raw = String::new();
    match path {
        Some(p) => {
            raw = std::fs::read_to_string(&p).unwrap_or_else(|e| {
                eprintln!("upkeep-parity: {p}: {e}");
                std::process::exit(1);
            })
        }
        None => {
            let _ = std::io::stdin().read_to_string(&mut raw);
        }
    }

    // BTreeMap: datasets in sorted order, matching the Python's `for ds in sorted(...)`.
    // Insertion order within a dataset is preserved, which is what the grouping consumes.
    let mut by_ds: BTreeMap<String, Vec<Fact>> = BTreeMap::new();
    for line in raw.lines() {
        if line.is_empty() {
            continue;
        }
        let mut it = line.splitn(3, '\t');
        let (Some(ds), Some(id), Some(text)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        by_ds
            .entry(ds.to_string())
            .or_default()
            .push(Fact { id: id.to_string(), text: unescape(text) });
    }

    let mut out = String::new();
    for (ds, facts) in &by_ds {
        for g in tidy_groups(facts, 12) {
            let ids: Vec<&str> = g.iter().map(|f| f.id.as_str()).collect();
            out.push_str(&format!("{ds}\t{}\n", ids.join(",")));
        }
    }
    print!("{out}");
}
