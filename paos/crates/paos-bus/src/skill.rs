//! The protocol version and its changelog — both read from SKILL.md.
//!
//! This is the fleet's self-heal for its own instructions: a session records the skill
//! version it last acknowledged, and when SKILL.md moves ahead it is told exactly what
//! changed and to re-read the file. Without it a session keeps following a protocol that
//! no longer exists — silently, because nothing about following stale instructions
//! errors.
//!
//! **ONE FILE.** The version lives in SKILL.md's frontmatter and the changelog used to
//! live in `bus_facet.py`, so two files had to agree and a test in a THIRD enforced it.
//! Both now come from SKILL.md, so there is nothing left to drift and the guard reads the
//! same file it guards.

use std::path::{Path, PathBuf};

/// Where the deployed skill lives. `PAOS_SKILL_DIR` overrides it for tests.
pub fn skill_md() -> PathBuf {
    if let Ok(d) = std::env::var("PAOS_SKILL_DIR") {
        if !d.trim().is_empty() {
            return PathBuf::from(d).join("SKILL.md");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".claude/skills/paos/SKILL.md")
}

/// The `version:` from the frontmatter.
///
/// Only the first 20 lines are scanned, as the Python did: further down, a line beginning
/// `version:` is prose, and treating it as the protocol version would tell every session
/// it was out of date.
pub fn version_of(text: &str) -> Option<String> {
    text.lines().take(20).find_map(|l| {
        l.strip_prefix("version:").map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
    })
}

/// The changelog: `(version, note)` pairs, in file order.
///
/// Delimited by the `changelog:begin`/`end` markers rather than "every `- N:` line in the
/// file", so an ordinary numbered list elsewhere in the skill cannot be mistaken for a
/// protocol note.
pub fn changelog_of(text: &str) -> Vec<(String, String)> {
    let Some(start) = text.find("<!-- changelog:begin -->") else { return Vec::new() };
    let rest = &text[start..];
    let end = rest.find("<!-- changelog:end -->").unwrap_or(rest.len());
    rest[..end]
        .lines()
        .filter_map(|l| {
            let l = l.trim().strip_prefix("- ")?;
            let (v, note) = l.split_once(':')?;
            let v = v.trim();
            if v.is_empty() || !v.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            Some((v.to_string(), note.trim().to_string()))
        })
        .collect()
}

/// Read SKILL.md once and return `(version, changelog)`.
pub fn read(path: &Path) -> (Option<String>, Vec<(String, String)>) {
    match std::fs::read_to_string(path) {
        Ok(t) => (version_of(&t), changelog_of(&t)),
        Err(_) => (None, Vec::new()),
    }
}

/// The drift notice for a session that last acknowledged `seen`, or `None` when current.
///
/// Lists only the versions BETWEEN what was seen and what is current — a session does not
/// need the history of changes it already adopted, and this prints on a live turn where it
/// competes with real work.
pub fn notice(seen: Option<&str>, current: &str, changelog: &[(String, String)]) -> Option<String> {
    let cur: i64 = current.parse().ok()?;
    let lo: i64 = match seen {
        Some(s) => s.parse().ok()?,
        // Never acknowledged anything: tell it to read the file, without replaying the
        // entire history at a session that has no context for it.
        None => return Some(format!(
            "\u{26a0} protocol v{current} — read ~/.claude/skills/paos/SKILL.md")),
    };
    if lo >= cur {
        return None;
    }
    let mut out = format!(
        "\u{26a0} protocol v{lo} \u{2192} v{cur} — re-read ~/.claude/skills/paos/SKILL.md");
    for (v, note) in changelog {
        if let Ok(n) = v.parse::<i64>() {
            if n > lo && n <= cur {
                out.push_str(&format!("\n    \u{2022} {note}"));
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---\nname: paos\nversion: 42\ndescription: x\n---\n\n\
        # body\n\n<!-- changelog:begin -->\n\
        - 40: daemon moved to Rust\n\
        - 41: cognee retired\n\
        - 42: run doctor first\n\
        <!-- changelog:end -->\n";

    #[test]
    fn the_version_comes_from_the_frontmatter() {
        assert_eq!(version_of(SAMPLE).as_deref(), Some("42"));
    }

    #[test]
    fn a_version_line_far_down_the_file_is_not_the_protocol_version() {
        // Prose like "version: whatever" in the body would otherwise tell every session it
        // was out of date. The Python scanned the first 20 lines for the same reason.
        let mut t = String::from("---\nname: paos\nversion: 42\n---\n");
        t.push_str(&"filler\n".repeat(40));
        t.push_str("version: 99\n");
        assert_eq!(version_of(&t).as_deref(), Some("42"));
    }

    #[test]
    fn the_changelog_is_bounded_by_its_markers() {
        // An ordinary numbered list elsewhere in the skill must not be read as protocol
        // notes — SKILL.md is 485 lines of prose containing plenty of lists.
        let mut t = String::from(SAMPLE);
        t.push_str("\n## Something else\n- 99: not a protocol note\n");
        let c = changelog_of(&t);
        assert_eq!(c.len(), 3);
        assert!(!c.iter().any(|(v, _)| v == "99"));
    }

    #[test]
    fn notice_lists_only_what_changed_since_the_session_acknowledged() {
        let c = changelog_of(SAMPLE);
        let n = notice(Some("40"), "42", &c).expect("drift");
        assert!(n.contains("v40 → v42"));
        assert!(n.contains("cognee retired"), "{n}");
        assert!(n.contains("run doctor first"), "{n}");
        assert!(!n.contains("daemon moved to Rust"),
                "40 was already acknowledged, so it must not be replayed: {n}");
    }

    #[test]
    fn a_current_session_is_told_nothing() {
        let c = changelog_of(SAMPLE);
        assert!(notice(Some("42"), "42", &c).is_none());
        // ...and a session somehow AHEAD is also not nagged.
        assert!(notice(Some("43"), "42", &c).is_none());
    }

    #[test]
    fn a_session_that_never_acknowledged_is_told_to_read_the_file() {
        let c = changelog_of(SAMPLE);
        let n = notice(None, "42", &c).expect("first-run notice");
        assert!(n.contains("v42") && n.contains("SKILL.md"), "{n}");
        // Not the whole history: it has no context for changes it never followed.
        assert!(!n.contains("cognee retired"), "{n}");
    }

    #[test]
    fn unparseable_input_is_silent_rather_than_wrong() {
        assert_eq!(version_of("no frontmatter here"), None);
        assert!(changelog_of("no markers").is_empty());
        assert!(notice(Some("x"), "42", &[]).is_none());
    }

    // ---- the guard that used to live in test_bus_facet.py ----

    #[test]
    fn the_deployed_skill_documents_its_own_version() {
        // THE RULE THIS REPLACES: `SKILL.md` carries a `version:` and every bump needs a
        // matching changelog entry, because running sessions self-heal by re-reading the
        // skill when the version moves. It was enforced by a Python test against a dict in
        // a THIRD file; both now come from SKILL.md, so the guard reads the file it guards.
        //
        // Located from the repo, not from $HOME: the deployed copy may lag the source, and
        // a guard that checks the deployed one would pass while the change being reviewed
        // is still wrong.
        // Two layouts, because there are two: inside the dotfiles repo the skill lives
        // under `dotfiles/.claude/skills/paos/`, and in the extracted public repo it is
        // `skill/`. The guard's own message said it must move with the skill rather than
        // silently stop checking — so it knows both rather than picking one, and fails
        // loudly if it finds neither.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let candidates = [
            root.join("dotfiles/.claude/skills/paos/SKILL.md"),
            root.join("skill/SKILL.md"),
        ];
        let Some(text) = candidates.iter().find_map(|p| std::fs::read_to_string(p).ok())
        else {
            panic!("SKILL.md not found at any of {:?} — if the skill moved again, this \
                    guard must move with it rather than silently stop checking",
                   candidates.iter().map(|p| p.display().to_string()).collect::<Vec<_>>());
        };
        let version = version_of(&text).expect("SKILL.md has a version: in its frontmatter");
        let changelog = changelog_of(&text);

        // Floor, so a broken scan cannot pass vacuously.
        assert!(changelog.len() >= 5,
                "only {} changelog entries parsed — the markers or the format have moved \
                 and this guard has stopped checking", changelog.len());

        assert!(changelog.iter().any(|(v, _)| *v == version),
                "SKILL.md is version {version} but has no changelog entry for it. A session \
                 waking on this version would be told the protocol moved and NOT what \
                 changed. Add a one-line entry between the changelog markers.");

        for (v, note) in &changelog {
            assert!(!note.is_empty(), "changelog entry {v} has no note");
            assert!(!note.contains('\n'), "changelog entry {v} must stay one line");
        }
    }
}

