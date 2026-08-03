//! Print the prompt constants, or the assembled prompts, for a byte-for-byte diff.
//!
//! Counterpart to `paos/parity/prompt_parity.py`. Same delimiters, same order.

use paos_librarian::prompts;
use std::io::Read;

fn sys_for(name: &str) -> Option<&'static str> {
    Some(match name {
        "_DISTILL_SYS" => prompts::DISTILL_SYS,
        "_TIDY_SYS" => prompts::TIDY_SYS,
        "_SPLIT_SYS" => prompts::SPLIT_SYS,
        "_LESSON_SYS" => prompts::LESSON_SYS,
        _ => return None,
    })
}

const NAMES: [&str; 4] = ["_DISTILL_SYS", "_TIDY_SYS", "_SPLIT_SYS", "_LESSON_SYS"];

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
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("raw") => {
            let mut out = String::new();
            for n in NAMES {
                out.push_str(&format!("<<<{n}>>>\n"));
                out.push_str(sys_for(n).unwrap());
                out.push_str("\n<<<END>>>\n");
            }
            print!("{out}");
        }
        Some("assembled") => {
            let mut raw = String::new();
            if std::io::stdin().read_to_string(&mut raw).is_err() {
                eprintln!("prompt-parity: cannot read stdin");
                std::process::exit(1);
            }
            let mut out = String::new();
            for line in raw.lines() {
                let (name, payload) = line.split_once('\t').unwrap_or((line, ""));
                let Some(system) = sys_for(name) else {
                    eprintln!("prompt-parity: unknown prompt {name}");
                    std::process::exit(1);
                };
                out.push_str(&format!("<<<PROMPT {name}>>>\n"));
                out.push_str(&paos_librarian::llm::assemble_claude_prompt(
                    system,
                    &unescape(payload),
                ));
                out.push_str("\n<<<END>>>\n");
            }
            print!("{out}");
        }
        _ => {
            eprintln!("usage: prompt-parity raw | prompt-parity assembled < pairs");
            std::process::exit(2);
        }
    }
}
