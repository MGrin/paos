//! `paos trajectory` — normalize local agent sessions for memory.
//!
//! Entirely local, like `gc`: reading ~/.claude/projects is machine-local work, and
//! routing it through the socket would make it unavailable from inside a sandbox for no
//! benefit. The consumer is `paos memory dream` / `paos memory lessons`, not a human.

use paos_trajectory as tj;

const USAGE: &str = "\
usage: paos trajectory <command>

  list [--limit N] [--since S] [--cursor C] [--json]
                        local Claude Code sessions, newest first
  show <session> [--json] [--no-truncate]
                        normalized records for one session
  stats [--limit N] [--since S]
                        native vs normalized token estimate
";

fn flag_value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn run(positional: &[String], args: &[String]) -> i32 {
    let Some(sub) = positional.get(1).map(String::as_str) else {
        print!("{USAGE}");
        return 0;
    };
    match sub {
        "list" => cmd_list(args),
        "show" => cmd_show(positional, args),
        "stats" => cmd_stats(args),
        other => {
            eprintln!("trajectory: unknown command '{other}'");
            print!("{USAGE}");
            2
        }
    }
}

fn limit_of(args: &[String], default: usize) -> usize {
    flag_value(args, "--limit").and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn cmd_list(args: &[String]) -> i32 {
    let listing = match tj::list_trajectories(
        "claude-code",
        limit_of(args, 20),
        flag_value(args, "--cursor").as_deref(),
        flag_value(args, "--since").as_deref(),
        None,
        now_secs(),
    ) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("trajectory: {e}");
            return 1;
        }
    };

    if has_flag(args, "--json") {
        let v = serde_json::to_value(&listing).unwrap_or(serde_json::Value::Null);
        println!("{}", tj::json::dumps_pretty(&v));
        return 0;
    }
    if listing.items.is_empty() {
        println!("no sessions found");
        return 0;
    }
    for it in &listing.items {
        // Python's "%-40s %s  %6dL  %s" — the width is on the id, and the title falls
        // back to the project directory.
        println!(
            "{:<40} {}  {:>6}L  {}",
            it.id,
            it.updated_at.as_deref().unwrap_or("?"),
            it.num_lines,
            it.title.as_deref().unwrap_or(&it.project)
        );
    }
    if let Some(c) = &listing.next_cursor {
        println!("… more (--cursor {c})");
    }
    0
}

fn cmd_show(positional: &[String], args: &[String]) -> i32 {
    let Some(session) = positional.get(2) else {
        eprintln!("trajectory: show needs <session>");
        return 2;
    };
    let path = match tj::resolve_session(session, None) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("trajectory: {e}");
            return 1;
        }
    };
    let raw = match std::fs::read(&path) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) => {
            eprintln!("trajectory: {path}: {e}");
            return 1;
        }
    };
    let trunc = if has_flag(args, "--no-truncate") { 0 } else { tj::DEFAULT_TRUNCATE };
    let out = match tj::normalize_transcript("claude-code", &raw, trunc) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("trajectory: {e}");
            return 1;
        }
    };
    if has_flag(args, "--json") {
        let v = serde_json::to_value(&out).unwrap_or(serde_json::Value::Null);
        println!("{}", tj::json::dumps_pretty(&v));
    } else {
        println!("{}", tj::render_text(&out.records, trunc));
    }
    0
}

fn cmd_stats(args: &[String]) -> i32 {
    let listing = match tj::list_trajectories(
        "claude-code",
        limit_of(args, 20),
        None,
        flag_value(args, "--since").as_deref(),
        None,
        now_secs(),
    ) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("trajectory: {e}");
            return 1;
        }
    };
    let (mut native, mut norm, mut n) = (0usize, 0usize, 0usize);
    for it in &listing.items {
        let Ok(bytes) = std::fs::read(&it.path) else { continue };
        let raw = String::from_utf8_lossy(&bytes);
        native += tj::estimate_tokens(&raw);
        if let Ok(out) = tj::normalize_transcript("claude-code", &raw, tj::DEFAULT_TRUNCATE) {
            norm += tj::estimate_tokens(&tj::render_text(&out.records, tj::DEFAULT_TRUNCATE));
        }
        n += 1;
    }
    let factor = if norm > 0 { native as f64 / norm as f64 } else { 0.0 };
    println!("sampled {n} session(s)");
    println!("native ~{native} tok · trajectory ~{norm} tok · {factor:.1}x reduction");
    0
}
