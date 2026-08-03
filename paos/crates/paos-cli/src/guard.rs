//! Decision logic for the PreToolUse(Bash) guard.
//!
//! Fail-OPEN by design: anything unparseable or unmatched is ALLOWED. Only unambiguous
//! footguns are blocked, and only when they appear as *executable command text* — never
//! when they merely appear inside a quoted argument.
//!
//! That distinction is the whole point of this module. A shell-only version that
//! substring-matched the raw command blocked *documentation about* a footgun — writing
//! `paos bus send lobby "never launch ... wait-joined with '&'"` was refused as if it
//! were the footgun. Prose describing a command is not a command.
//!
//! Ported from `bash_guard.py`. Two deliberate differences from that source, both
//! recorded because a silent one would read as a porting mistake:
//!
//!   1. Heredoc scanning splits on '\n' only. Python's `splitlines()` also breaks on
//!      U+2028, U+2029, \x85, \v, \f and \x1c-\x1e — none of which a shell treats as a
//!      line terminator, so a heredoc body containing one could end its scan early and
//!      change the verdict. `split('\n')` is what the shell actually does.
//!   2. The regexes are the `regex` crate rather than hand-rolled matchers. The
//!      catastrophic patterns are the security-critical half; a hand-rolled matcher that
//!      diverged would fail OPEN on a real `rm -rf /`, which is the one direction this
//!      module must never fail in.

use regex::Regex;
use std::sync::OnceLock;

pub const CATASTROPHIC_MSG: &str =
    "bash-guard: refused a catastrophic command pattern (root/home wipe, mkfs, \
     dd-to-device). Run it yourself if intentional.";

pub const WAKE_LOOP_MSG: &str =
    "bash-guard: refused 'paos bus wait[-joined]' launched with a shell background '&'. \
     It detaches the listener from the Claude Code harness (no re-invoke on message \
     delivery) and holds the singleton lock. Use the Bash tool's run_in_background=true \
     parameter with NO trailing '&'.";

pub const BACKTICK_MSG: &str = "bash-guard: refused a `paos` command with an UNQUOTED BACKTICK.\n\
     The shell runs command substitution on it before paos ever sees the text, so the \
     backticked span is replaced by command output (usually empty) and the message or \
     fact is stored GUTTED — with exit code 0 and no warning anywhere.\n\
     Fix: wrap the argument in SINGLE quotes ('...'), where backticks are literal. If it \
     must be double-quoted, escape them (\\`). To quote code, prefer single quotes or a \
     heredoc.";

/// Blank out single- and double-quoted spans, preserving structure outside them.
///
/// Unterminated quotes blank to end of string — text a shell would treat as one long
/// argument anyway. Backslash-escapes are honoured inside double quotes only, matching
/// sh semantics.
pub fn strip_quoted(cmd: &str) -> String {
    // Indexed over chars, not bytes: the Python counts characters, and a multi-byte
    // character in a quoted span would otherwise shift every subsequent index.
    let ch: Vec<char> = cmd.chars().collect();
    let mut out = String::new();
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < ch.len() {
        let c = ch[i];
        match quote {
            None => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                } else {
                    out.push(c);
                }
            }
            Some(q) => {
                if c == '\\' && q == '"' && i + 1 < ch.len() {
                    i += 1; // skip the escaped char inside "..."
                } else if c == q {
                    quote = None;
                }
            }
        }
        i += 1;
    }
    out
}

/// Remove the bodies of heredocs whose delimiter is QUOTED (`<<'EOF'`, `<<"EOF"`, `<<\EOF`).
///
/// A quoted delimiter turns off every expansion for the whole body, so backticks in there
/// are literal — exactly like single quotes, and for the same reason. Without this the
/// backtick rule blocks `git commit -F - <<'EOF'` whenever the message mentions paos and
/// quotes any code, which is a legitimate and common command.
///
/// An UNQUOTED `<<EOF` does expand, so its body is deliberately left in.
fn strip_quoted_heredocs(cmd: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"<<-?\s*(?:'([^']+)'|"([^"]+)"|\\(\w+))"#).expect("heredoc regex")
    });
    let lines: Vec<&str> = cmd.split('\n').collect();
    let mut out: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        out.push(lines[i]);
        let caps = re.captures(lines[i]);
        i += 1;
        let Some(caps) = caps else { continue };
        let delim = caps
            .get(1)
            .or_else(|| caps.get(2))
            .or_else(|| caps.get(3))
            .map(|m| m.as_str())
            .unwrap_or("");
        while i < lines.len() && lines[i].trim() != delim {
            i += 1; // body dropped: quoted delimiter means no expansion
        }
        if i < lines.len() {
            out.push(lines[i]);
            i += 1;
        }
    }
    out.join("\n")
}

/// The parts of `cmd` a shell would perform command substitution in.
///
/// Single-quoted spans are excluded because a shell treats everything inside them
/// literally; double-quoted and unquoted text is included because it does not. That
/// distinction is the entire rule — the same backtick is harmless in '...' and
/// destructive in "...".
pub fn substitutable_spans(cmd: &str) -> String {
    let cmd = strip_quoted_heredocs(cmd);
    let ch: Vec<char> = cmd.chars().collect();
    let mut out = String::new();
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < ch.len() {
        let c = ch[i];
        match quote {
            None => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                } else {
                    out.push(c);
                }
            }
            Some('\'') => {
                if c == '\'' {
                    quote = None; // contents deliberately dropped: literal, safe
                }
            }
            Some(_) => {
                // inside "..."
                if c == '\\' && i + 1 < ch.len() {
                    i += 1; // \` is escaped — not a substitution
                } else if c == '"' {
                    quote = None;
                } else {
                    out.push(c);
                }
            }
        }
        i += 1;
    }
    out
}

/// A `paos` invocation whose text will be silently rewritten by the shell.
///
/// Scoped to paos because that is where the damage is invisible AND durable: the
/// substituted text is what gets stored as a fact or posted to a room, so the content is
/// lost with exit code 0 and nothing to notice later. A memory warning about this has
/// existed since 2026-07-27 and sessions kept doing it anyway — including the one that
/// wrote the warning. A trap a correct, well-informed reader still falls into is not a
/// knowledge problem, so this refuses instead of reminding.
///
/// Only backticks. `$(...)` is flagged nowhere: it is what someone writes when they MEAN
/// to substitute, while a backtick in a paos message is almost always markdown.
pub fn is_gutting_paos_call(cmd: &str) -> bool {
    if !strip_quoted(cmd).contains("paos") {
        return false; // `paos` only inside quotes = prose about paos
    }
    substitutable_spans(cmd).contains('`')
}

/// True if `text` contains a shell background '&' — after removing the operators that
/// merely happen to contain one.
fn has_shell_background(text: &str) -> bool {
    let mut t = text.to_string();
    for tok in ["&&", "2>&1", "1>&2", ">&2", ">&", "$&"] {
        t = t.replace(tok, "");
    }
    t.contains('&')
}

/// Does this command contain an unambiguous machine-killer?
///
/// Matched against text a shell would EXECUTE, with the bodies of quoted heredocs removed
/// first — and only those. A `<<'EOF'` body is data by definition: the shell performs no
/// expansion in it and never runs it, so a commit message, a doc or a bus message that
/// merely NAMES a root wipe is prose, not a command.
///
/// That distinction is the module's stated purpose and this check did not honour it. It
/// refused three legitimate commands in ten minutes while this file was being written:
/// twice for test fixtures naming the patterns, and once for the commit message describing
/// the change. A guard people have to work around is one they learn to route around.
///
/// Everything else is still matched RAW, including single- and double-quoted arguments.
/// That is deliberate and asymmetric with the backtick rule: `eval "rm -rf /"` executes,
/// so quoting must not be an escape hatch here the way it legitimately is there.
pub fn is_catastrophic(cmd: &str) -> bool {
    is_catastrophic_raw(&strip_quoted_heredocs_bodies(cmd))
}

/// Remove the BODY of every quoted heredoc, keeping the surrounding command intact.
///
/// Distinct from [`strip_quoted_heredocs`], which preserves the delimiter lines because the
/// backtick scanner needs the structure around them. Here the body is simply dropped.
fn strip_quoted_heredocs_bodies(cmd: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"<<-?\s*(?:'([^']+)'|"([^"]+)"|\\(\w+))"#).expect("heredoc regex")
    });
    let lines: Vec<&str> = cmd.split('\n').collect();
    let mut out: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        out.push(lines[i]);
        let caps = re.captures(lines[i]);
        i += 1;
        let Some(caps) = caps else { continue };
        let delim = caps.get(1).or_else(|| caps.get(2)).or_else(|| caps.get(3))
            .map(|m| m.as_str()).unwrap_or("");
        while i < lines.len() && lines[i].trim() != delim {
            i += 1;
        }
        if i < lines.len() {
            out.push(lines[i]);
            i += 1;
        }
    }
    out.join("\n")
}

fn is_catastrophic_raw(cmd: &str) -> bool {
    static RM: OnceLock<Regex> = OnceLock::new();
    static DD: OnceLock<Regex> = OnceLock::new();
    let rm = RM.get_or_init(|| {
        // A QUOTE counts as both an opener and a terminator, which the ported pattern
        // missed on both ends. `eval "rm -rf /"` and `rm -rf "/"` were ACCEPTED — measured,
        // not suspected — because the target had to be followed by whitespace or
        // end-of-string, and a closing quote is neither. Both of those wipe a disk.
        //
        // Still anchored on the target itself, so `rm -rf /foo` and `rm -rf ./build`
        // remain untouched: the character after the target must end it, not continue it.
        Regex::new(r#"rm\s+-[a-zA-Z]*[rf][a-zA-Z]*\s+["']?(/|~|"?\$HOME"?)(\s*$|\s|/?\*|["'])"#)
            .expect("rm regex")
    });
    let dd = DD.get_or_init(|| Regex::new(r"dd\s+.*of=/dev/").expect("dd regex"));
    rm.is_match(cmd) || cmd.contains("mkfs") || dd.is_match(cmd)
}

/// A `paos bus wait[-joined]` launched with a shell background '&'.
///
/// Both the invocation AND the '&' must be *outside* quotes: a message body that talks
/// about either one is documentation, not an invocation.
pub fn is_detached_listener(cmd: &str) -> bool {
    let bare = strip_quoted(cmd);
    bare.contains("paos bus wait") && has_shell_background(&bare)
}

/// `(exit_code, message)`. 0 = allow, 2 = block.
pub fn decide(cmd: &str) -> (i32, &'static str) {
    if cmd.is_empty() {
        return (0, "");
    }
    if is_catastrophic(cmd) {
        return (2, CATASTROPHIC_MSG);
    }
    if is_detached_listener(cmd) {
        return (2, WAKE_LOOP_MSG);
    }
    if is_gutting_paos_call(cmd) {
        return (2, BACKTICK_MSG);
    }
    (0, "")
}

/// Is this path inside the disabled `~/.claude` file-memory store?
///
/// That store is deliberately off on this machine — durable memory routes to `paos
/// memory` — and a session writing to it believes it has remembered something when
/// nothing durable happened. Blocking is the point.
///
/// Matches `<anything>/.claude/projects/<id>/memory` and anything beneath it, the same
/// shape the shell regex used.
pub fn is_disabled_file_memory(path: &str) -> bool {
    let Some(rest) = path.split("/.claude/projects/").nth(1) else { return false };
    // Exactly one path segment for the project id, then `memory` as the next segment.
    let mut parts = rest.splitn(3, '/');
    let (_id, seg) = (parts.next(), parts.next());
    matches!(seg, Some("memory"))
}

/// Hook entry point for the memory guard: block writes to the disabled file-memory store.
///
/// Was a `python3 -c` inside memory-guard.sh, parsing one JSON field on EVERY Write, Edit
/// and MultiEdit — 28.4 ms of interpreter boot per call to read `tool_input.file_path`.
/// The same trade the Bash guard had, on a path that fires just as often.
pub fn run_memory_guard() -> i32 {
    use std::io::Read;
    let mut s = String::new();
    if std::io::stdin().read_to_string(&mut s).is_err() {
        return 0; // fail-OPEN
    }
    let path = serde_json::from_str::<serde_json::Value>(&s)
        .ok()
        .and_then(|v| {
            v.get("tool_input")
                .and_then(|t| t.get("file_path"))
                .and_then(|c| c.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    if path.is_empty() || !is_disabled_file_memory(&path) {
        return 0;
    }
    eprintln!("memory-guard: refused a write to the ~/.claude file-memory store ({path}).");
    eprintln!("This machine routes durable memory to paos memory (remember/recall). \
               See ~/.claude/CLAUDE.md.");
    2
}

/// Hook entry point: read the PreToolUse payload from stdin and decide.
pub fn run() -> i32 {
    use std::io::Read;
    let mut s = String::new();
    if std::io::stdin().read_to_string(&mut s).is_err() {
        return 0; // fail-OPEN
    }
    let cmd = serde_json::from_str::<serde_json::Value>(&s)
        .ok()
        .and_then(|v| {
            v.get("tool_input")
                .and_then(|t| t.get("command"))
                .and_then(|c| c.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    let (code, msg) = decide(&cmd);
    if !msg.is_empty() {
        eprintln!("{msg}");
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- the distinction the module exists for: prose is not a command ----

    #[test]
    fn documentation_about_a_footgun_is_not_the_footgun() {
        // The regression that motivated the rewrite: a message ABOUT the wake loop.
        let cmd = r#"paos bus send lobby "never launch paos bus wait-joined with '&'""#;
        assert_eq!(decide(cmd).0, 0);
    }

    #[test]
    fn an_actual_detached_listener_is_refused() {
        assert_eq!(decide("paos bus wait-joined &").0, 2);
        assert_eq!(decide("paos bus wait &").1, WAKE_LOOP_MSG);
    }

    #[test]
    fn operators_containing_an_ampersand_are_not_backgrounding() {
        // `&&` and redirections must not read as a background '&'.
        assert_eq!(decide("paos bus wait-joined && echo done").0, 0);
        assert_eq!(decide("paos bus wait-joined 2>&1").0, 0);
    }

    // ---- backticks: harmless in '...', destructive in "..." ----

    #[test]
    fn a_backtick_in_double_quotes_guts_the_message_and_is_refused() {
        assert_eq!(decide(r#"paos bus send lobby "run `ls` first""#).0, 2);
    }

    #[test]
    fn a_backtick_in_single_quotes_is_literal_and_allowed() {
        assert_eq!(decide(r#"paos bus send lobby 'run `ls` first'"#).0, 0);
    }

    #[test]
    fn an_escaped_backtick_in_double_quotes_is_allowed() {
        assert_eq!(decide(r#"paos bus send lobby "run \` first""#).0, 0);
    }

    #[test]
    fn a_quoted_heredoc_body_may_contain_backticks() {
        // This exact shape blocked the guard's own commit message, which is how the
        // gap was found: a quoted delimiter disables every expansion.
        let cmd = "paos memory remember --global <<'EOF'\nuse `paos bus send` here\nEOF";
        assert_eq!(decide(cmd).0, 0);
    }

    #[test]
    fn an_unquoted_heredoc_body_still_expands_so_it_is_refused() {
        let cmd = "paos memory remember --global <<EOF\nuse `paos bus send` here\nEOF";
        assert_eq!(decide(cmd).0, 2);
    }

    #[test]
    fn a_backtick_in_a_non_paos_command_is_not_our_business() {
        // Scoped to paos: elsewhere a backtick is ordinary shell, not silent data loss.
        assert_eq!(decide("echo `date`").0, 0);
    }

    #[test]
    fn the_word_paos_only_inside_quotes_is_prose_not_an_invocation() {
        assert_eq!(decide(r#"echo "paos is great" && echo `date`"#).0, 0);
    }

    // ---- catastrophic ----

    #[test]
    fn root_and_home_wipes_are_refused() {
        for cmd in ["rm -rf /", "rm -rf /*", "rm -rf ~", "rm -rf $HOME",
                    "rm -rf \"$HOME\"", "rm -fr / ", "sudo rm -rf /"] {
            assert_eq!(decide(cmd).0, 2, "should refuse: {cmd}");
        }
    }

    #[test]
    fn a_trailing_newline_does_not_smuggle_a_root_wipe_past_the_guard() {
        // Python's `$` also matches before a trailing newline; Rust's does not. `\s*`
        // absorbs it either way, and this asserts that rather than assuming it.
        assert_eq!(decide("rm -rf /\n").0, 2);
    }

    #[test]
    fn mkfs_and_dd_to_a_device_are_refused() {
        assert_eq!(decide("mkfs.ext4 /dev/disk2").0, 2);
        assert_eq!(decide("dd if=/dev/zero of=/dev/disk2").0, 2);
    }

    #[test]
    fn a_quoted_heredoc_body_naming_a_wipe_is_prose_not_a_command() {
        // Measured, not imagined: this guard refused three legitimate commands in ten
        // minutes while it was being written — two test fixtures and the commit message
        // describing the change. A `<<'EOF'` body is data; the shell expands nothing in it
        // and never executes it.
        let msg = "git commit -F - <<'EOF'\nrefactor: stop the bootstrap being unguarded \
                   against rm -rf / and mkfs\nEOF";
        assert_eq!(decide(msg).0, 0, "a commit message about a wipe is not a wipe");

        let fixture = "cat > t.bats <<'EOF'\n  run guard 'rm -rf /'\n  run guard 'mkfs.ext4 /dev/disk2'\nEOF";
        assert_eq!(decide(fixture).0, 0, "test fixtures in a heredoc are data");
    }

    #[test]
    fn quoting_alone_is_still_not_an_escape_hatch() {
        // The asymmetry with the backtick rule is deliberate: `eval "rm -rf /"` EXECUTES,
        // so a quoted argument must still be refused. Only a quoted-heredoc BODY is exempt,
        // because that is the one construct a shell guarantees it will not run.
        // These two were ACCEPTED before this commit — the pattern required whitespace or
        // end-of-string after the target, and a closing quote is neither. Both wipe a disk.
        assert_eq!(decide(r#"eval "rm -rf /""#).0, 2);
        assert_eq!(decide(r#"rm -rf "/""#).0, 2);
        assert_eq!(decide("echo 'rm -rf /' | sh").0, 2);
        // And an UNQUOTED heredoc still expands, so its body is live text.
        assert_eq!(decide("sh <<EOF\nrm -rf /\nEOF").0, 2);
    }

    #[test]
    fn scoped_removals_are_allowed() {
        // Fail-open matters: over-blocking ordinary work is its own failure.
        for cmd in ["rm -rf ./build", "rm -rf /tmp/scratch", "rm -rf target",
                    "rm file.txt", "dd if=a of=b"] {
            assert_eq!(decide(cmd).0, 0, "should allow: {cmd}");
        }
    }

    // ---- fail-open edges ----

    #[test]
    fn an_empty_command_is_allowed() {
        assert_eq!(decide("").0, 0);
    }

    #[test]
    fn an_unterminated_quote_does_not_panic() {
        assert_eq!(decide(r#"paos bus send lobby "unterminated"#).0, 0);
        assert_eq!(decide("paos bus send lobby 'unterminated").0, 0);
    }

    #[test]
    fn a_multibyte_character_does_not_shift_the_quote_scanner() {
        // Python indexes by character; a byte-indexed port would mis-locate the closing
        // quote after any non-ASCII text and could flip the verdict.
        let cmd = r#"paos bus send lobby "héllo — wörld `ls`""#;
        assert_eq!(decide(cmd).0, 2, "the backtick is still substitutable");
        let safe = r#"paos bus send lobby 'héllo — wörld `ls`'"#;
        assert_eq!(decide(safe).0, 0, "single quotes still literal");
    }

    #[test]
    fn an_exotic_separator_does_not_end_a_heredoc_the_way_python_thinks_it_does() {
        // THE DELIBERATE DIVERGENCE, asserted rather than left to be rediscovered as a
        // suspected porting bug. Measured: bash_guard.py returns 2 here, this returns 0.
        //
        // Python's splitlines() treats the U+2028 as a line break, so it sees a line
        // reading "EOF", ends the heredoc early, and judges the trailing `echo` to be
        // substitutable command text. A shell does not: U+2028 is an ordinary character,
        // the heredoc runs to the real EOF, and that `echo` is literal body text. Allowing
        // it is correct, and blocking it is the guard refusing a legitimate command.
        //
        // Only 2 of 51,301 real commands reach this branch at all, which is exactly why
        // it needs a named test — the corpus diff would never have caught a regression.
        let cmd = "paos memory remember <<'EOF'\nliteral `ls` text\u{2028}EOF\necho `date`";
        assert_eq!(decide(cmd).0, 0);
    }

    #[test]
    fn a_trailing_backslash_inside_double_quotes_does_not_run_off_the_end() {
        assert_eq!(decide(r#"paos bus send lobby "trailing \"#).0, 0);
    }

    #[test]
    fn the_disabled_file_memory_store_is_matched_the_way_the_shell_regex_did() {
        // Real shape: ~/.claude/projects/<project-id>/memory/<file>, plus MEMORY.md.
        assert!(is_disabled_file_memory("/Users/example/.claude/projects/-Users-x/memory/a.md"));
        assert!(is_disabled_file_memory("/Users/example/.claude/projects/abc/memory"));
        assert!(is_disabled_file_memory("/Users/example/.claude/projects/abc/memory/MEMORY.md"));
    }

    #[test]
    fn ordinary_paths_are_not_blocked_including_near_misses() {
        // Over-blocking here stops real edits. `memory` must be the segment right after the
        // project id — not anywhere in the path, or every file in a repo called "memory"
        // becomes unwritable.
        assert!(!is_disabled_file_memory("/Users/example/Dev/proj/src/memory/store.rs"));
        assert!(!is_disabled_file_memory("/Users/example/.claude/projects/abc/other/memory"));
        assert!(!is_disabled_file_memory("/Users/example/.claude/skills/paos/SKILL.md"));
        assert!(!is_disabled_file_memory("/Users/example/.claude/projects/abc/notes.md"));
        assert!(!is_disabled_file_memory(""));
        // "memoryX" is a different directory and must stay writable.
        assert!(!is_disabled_file_memory("/x/.claude/projects/abc/memoryX/f.md"));
    }

}
