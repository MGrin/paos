//! Differential harness for the bash-guard port: reads `{"cmd": ...}` JSONL on stdin and
//! prints one `<index> <exit_code>` line per command, for diffing against `bash_guard.py`.
//!
//! A separate bin rather than a hidden CLI verb so the shipped command surface stays the
//! same, and so the whole corpus runs in ONE process — 51k subprocess spawns would make
//! the check slow enough that nobody would run it twice.
//!
//! `#[path]` pulls in the real module. Re-implementing or copying it here would test a
//! copy, and a harness that silently tests the wrong build makes a correct port look
//! broken and a broken one look correct.

// `run()` is the stdin hook entry point, used by the CLI and not by this harness.
#[allow(dead_code)]
#[path = "../guard.rs"]
mod guard;

use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    for (i, line) in stdin.lock().lines().enumerate() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let cmd = serde_json::from_str::<serde_json::Value>(&line)
            .ok()
            .and_then(|v| v.get("cmd").and_then(|c| c.as_str()).map(str::to_string))
            .unwrap_or_default();
        let _ = writeln!(out, "{i} {}", guard::decide(&cmd).0);
    }
}
