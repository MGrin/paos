//! `paos gc` — what is safe to delete, and what it would cost to get back.
//!
//! Ported from `gc_facet.py` (146 lines) as the first step of retiring the Python facets.
//! It runs entirely locally and never opens the socket: a `du` over a build cache is not
//! the daemon's work, and routing it through the daemon would block the single writer for
//! the duration of a multi-second filesystem scan.
//!
//! This REPORTS. It does not delete. Two reasons, both carried over verbatim from the
//! Python because they are the whole design:
//!
//!   * "Reclaimable" is not one category. `go-build` regenerates in minutes from nothing
//!     but CPU. `~/.lmstudio` is 36 GB of downloaded MODEL WEIGHTS — technically
//!     re-fetchable, practically an evening on a home connection, and useless to an
//!     offline machine that depends on them. Reporting both as "reclaimable" would invite
//!     exactly the wrong deletion.
//!   * Deleting is the one action here with no undo. Everything else paos does is
//!     proposed and human-gated; disk should be no different.

use std::path::{Path, PathBuf};

/// (label, path, how to reclaim, cost of regenerating).
///
/// `cheap` means CPU and minutes. Anything else is reported but never counted toward the
/// reclaimable total — that distinction is the entire point of the command.
const TARGETS: &[(&str, &str, &str, &str)] = &[
    ("go build cache", "~/Library/Caches/go-build", "go clean -cache", "cheap"),
    ("Homebrew downloads", "~/Library/Caches/Homebrew", "brew cleanup -s", "cheap"),
    ("uv cache", "~/.cache/uv", "uv cache clean", "cheap"),
    ("bun cache", "~/.bun/install/cache", "bun pm cache rm", "cheap"),
    ("npm cache", "~/.npm/_cacache", "npm cache clean --force", "cheap"),
    ("cargo registry", "~/.cargo/registry", "cargo cache -a  (or delete; re-downloads)", "cheap"),
    ("playwright browsers", "~/Library/Caches/ms-playwright",
     "playwright-cli install-browser chromium  (to restore)", "cheap"),
    ("LM Studio models", "~/.lmstudio", "delete individual models in the app",
     "EXPENSIVE — model weights, a long re-download, and this machine works offline"),
];

/// Build outputs under Conductor worktrees. Each Rust worktree carries its own ~1.9 GB
/// target/ because CARGO_TARGET_DIR is unset — worth naming, since one env var collapses
/// them all.
const WORKTREE_GLOBS: &[(&str, &str, &str)] = &[
    ("~/conductor/workspaces/*/*/paos/target", "cargo clean", "cheap"),
    ("~/conductor/workspaces/*/*/node_modules", "rm -rf (reinstall to restore)", "cheap"),
    ("~/conductor/workspaces/*/*/.next", "rm -rf", "cheap"),
];

pub struct Item {
    pub label: String,
    pub bytes: u64,
    pub reclaim: String,
    pub cost: String,
}

fn expand(p: &str) -> PathBuf {
    match std::env::var("HOME") {
        Ok(h) if p.starts_with("~/") => PathBuf::from(h).join(&p[2..]),
        _ => PathBuf::from(p),
    }
}

/// Size of a directory, or 0 if missing/unreadable.
///
/// Shells `du` rather than walking in Rust: on a 41 GB tree with hundreds of thousands of
/// files the difference is minutes, and this runs on demand in front of a human.
pub fn du_bytes(path: &Path) -> u64 {
    let Ok(out) = std::process::Command::new("du").arg("-sk").arg(path).output() else {
        return 0;
    };
    if !out.status.success() {
        return 0;
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .and_then(|k| k.parse::<u64>().ok())
        .map(|k| k * 1024)
        .unwrap_or(0)
}

/// Shell out for globbing rather than take a crate dependency for three patterns.
fn glob(pattern: &str) -> Vec<PathBuf> {
    let Ok(out) = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("ls -d {pattern} 2>/dev/null"))
        .output() else { return vec![] };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Every known reclaimable location that actually exists, largest first.
pub fn scan() -> Vec<Item> {
    let mut found = vec![];
    for (label, path, how, cost) in TARGETS {
        let p = expand(path);
        if !p.exists() {
            continue;   // a machine without Go must not be told it can reclaim a go cache
        }
        let b = du_bytes(&p);
        if b > 0 {
            found.push(Item { label: (*label).into(), bytes: b,
                              reclaim: (*how).into(), cost: (*cost).into() });
        }
    }
    for (pattern, how, cost) in WORKTREE_GLOBS {
        let matches = glob(&expand(pattern).to_string_lossy());
        let total: u64 = matches.iter().map(|m| du_bytes(m)).sum();
        if total > 0 {
            let name = Path::new(pattern).file_name()
                .map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            found.push(Item {
                label: format!("{} ({} worktree{})", name, matches.len(),
                               if matches.len() == 1 { "" } else { "s" }),
                bytes: total, reclaim: (*how).into(), cost: (*cost).into() });
        }
    }
    found.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    found
}

pub fn free_bytes() -> u64 {
    // $HOME directly, NOT expand("~"): expand only rewrites a leading "~/", so a bare "~"
    // reached df as a literal path, df failed, and the report led with "0 GB free" — on a
    // disk with 191 GB free. Caught by diffing against the Python this replaces.
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let Ok(out) = std::process::Command::new("df").arg("-k").arg(&home).output() else {
        return 0;
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .nth(1)
        .and_then(|l| l.split_whitespace().nth(3))
        .and_then(|k| k.parse::<u64>().ok())
        .map(|k| k * 1024)
        .unwrap_or(0)
}

fn gb(n: u64) -> f64 {
    n as f64 / 1e9
}

pub fn render(found: &[Item], free: u64) -> Vec<String> {
    if found.is_empty() {
        return vec!["nothing reclaimable found".into()];
    }
    let mut out = vec![format!("{:.0} GB free. Reclaimable:\n", gb(free))];
    for f in found {
        let flag = if f.cost == "cheap" { "  " } else { "! " };
        out.push(format!("{}{:<34} {:>6.1} GB   {}", flag, f.label, gb(f.bytes), f.reclaim));
    }
    let cheap: u64 = found.iter().filter(|f| f.cost == "cheap").map(|f| f.bytes).sum();
    out.push(format!("\n  cheap to regenerate: {:.0} GB total", gb(cheap)));
    for f in found.iter().filter(|f| f.cost != "cheap") {
        out.push(format!("  ! {}: {}", f.label, f.cost));
    }
    // Name the one-line structural fix rather than only the symptom.
    if found.iter().any(|f| f.label.contains("target")) {
        out.push("\n  Rust worktrees each keep their own target/. One env var collapses them:".into());
        out.push("    export CARGO_TARGET_DIR=\"$HOME/.cache/cargo-target\"".into());
    }
    out.push("\n  This command never deletes. Run the shown command yourself.".into());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_path_measures_zero_rather_than_crashing() {
        assert_eq!(du_bytes(Path::new("/definitely/not/here")), 0);
    }

    #[test]
    fn a_real_directory_measures_nonzero() {
        let d = std::env::temp_dir().join(format!("paos-gc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("f"), vec![b'x'; 100_000]).unwrap();
        assert!(du_bytes(&d) > 0);
    }

    #[test]
    fn model_weights_are_not_advertised_as_cheap() {
        // ~/.lmstudio is 36 GB and technically re-downloadable. Calling that "reclaimable"
        // next to a build cache invites exactly the wrong deletion, on a machine whose
        // local models are the point.
        let lms = TARGETS.iter().find(|t| t.0.contains("LM Studio")).unwrap();
        assert_ne!(lms.3, "cheap");
        assert!(lms.3.contains("EXPENSIVE"));
    }

    #[test]
    fn build_caches_are_cheap() {
        for label in ["go build cache", "Homebrew downloads", "uv cache"] {
            assert_eq!(TARGETS.iter().find(|t| t.0 == label).unwrap().3, "cheap", "{label}");
        }
    }

    #[test]
    fn every_target_names_how_to_reclaim_it() {
        // A size with no command is trivia; the operator should not have to look it up.
        for (label, _, how, _) in TARGETS {
            assert!(!how.trim().is_empty(), "{label}");
        }
    }

    #[test]
    fn an_expensive_item_is_not_counted_as_reclaimable() {
        let out = render(&[Item { label: "models".into(), bytes: 36_000_000_000,
                                  reclaim: "app".into(),
                                  cost: "EXPENSIVE — weights".into() }], 0);
        let joined = out.join("\n");
        assert!(joined.contains("EXPENSIVE"));
        assert!(joined.contains("cheap to regenerate: 0 GB"), "{joined}");
    }

    #[test]
    fn free_space_is_actually_measured() {
        // It reported "0 GB free" on a disk with 191 GB free, because expand() only
        // rewrites a leading "~/" and df was handed a literal "~".
        assert!(free_bytes() > 0, "free space must not silently read as zero");
    }

    #[test]
    fn the_report_states_that_it_never_deletes() {
        // The safety property should be visible in the output, not only in the docs.
        let out = render(&[Item { label: "x".into(), bytes: 1, reclaim: "rm".into(),
                                  cost: "cheap".into() }], 0);
        assert!(out.join("\n").contains("never deletes"));
    }
}
