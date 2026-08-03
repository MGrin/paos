//! `paosctl hook <name>` — the Claude Code hooks, in Rust.
//!
//! These were Python (`paos-init`, `session-presence`), and they are the hottest paths in
//! paos: `session-presence` runs on EVERY assistant turn in every session, and `paos-init`
//! runs three recalls at every session start. That is exactly the cost the Rust CLI was
//! built to remove — the Python paid ~37 ms of interpreter boot before doing anything.
//!
//! **The hook file on disk stays a two-line `sh` script that `exec`s this.** That is not
//! laziness, it is load-bearing:
//!
//!   * A compiled binary cannot live in git, and `dotfiles/.claude` is deployed by
//!     enumerating `git ls-files` — an untracked file is simply never copied.
//!   * The hook MUST remain a single simple command. settings.json invokes it as a bare
//!     path, so the shell exec-replaces its own image and `getppid()` is the real,
//!     long-lived session pid. The reaper uses that pid as a confirmed-death signal, so a
//!     FORKING wrapper (`sh -c "a; b"`, `&&`, a trailing `; true`, a pipe) would make
//!     every live session look dead. `exec` preserves the chain; anything else does not.
//!
//! Belt and braces, the wrapper also passes `$PPID` explicitly, so correctness does not
//! rest on exec semantics alone.
//!
//! FAIL-SAFE throughout: a hook must never block or error a session. Every path returns 0.

use std::io::Read;

/// Read the hook payload from stdin. Any failure yields an empty object — a hook that
/// cannot parse its input must still exit cleanly.
fn payload() -> serde_json::Value {
    let mut s = String::new();
    if std::io::stdin().read_to_string(&mut s).is_err() {
        return serde_json::Value::Null;
    }
    serde_json::from_str(&s).unwrap_or(serde_json::Value::Null)
}

fn field<'a>(v: &'a serde_json::Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(|x| x.as_str()).filter(|s| !s.is_empty())
}

pub fn run(positional: &[String], args: &[String]) -> i32 {
    // The session pid, passed by the wrapper. Falls back to our own parent, which is
    // correct when the wrapper used `exec`.
    let ppid = args.iter().position(|a| a == "--ppid")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| std::os::unix::process::parent_id().to_string());

    match positional.get(1).map(String::as_str).unwrap_or("") {
        "session-presence" => session_presence(&ppid),
        "paos-init" => paos_init(),
        "bash-guard" => crate::guard::run(),
        "memory-guard" => crate::guard::run_memory_guard(),
        other => {
            eprintln!("unknown hook: {other}");
            0                     // never fail a session, even on a bad invocation
        }
    }
}

/// The bus argv for one presence event; `None` for an event we do not handle.
///
/// Pure, and deliberately separate from the spawn, so the exact command line can be
/// asserted. This is the most dangerous line in the hook: `--ppid 4242` once landed in
/// the HANDLE slot instead of the pid slot, every unit test still passed, and it was
/// caught only by running the real argv rather than a fixture. A session minted with a
/// handle named after its ppid would have been near-undebuggable after the fact.
///
/// Verified equal to the Python hook (`session-presence`, `_run_paos` stubbed) for all
/// three events; Python prepends `bus` inside `_run_paos`, which is why it appears here.
pub fn presence_argv<'a>(event: &str, sid: &'a str, ppid: &'a str) -> Option<Vec<&'a str>> {
    Some(match event {
        "SessionStart" => vec!["bus", "session-start", "--session-id", sid, "--ppid", ppid],
        "SessionEnd" => vec!["bus", "session-end", "--session-id", sid],
        "Stop" => vec!["bus", "heartbeat", "--session-id", sid, "--ppid", ppid],
        _ => return None,
    })
}

/// Bind the bus identity to the Claude session id and drive presence.
///
///   SessionStart -> bus session-start (mint/bind the handle, mark online)
///   SessionEnd   -> bus session-end   (archive + cascade membership)
///   Stop         -> bus heartbeat     (advance last_seen)
fn session_presence(ppid: &str) -> i32 {
    let p = payload();
    let Some(event) = field(&p, "hook_event_name") else { return 0 };

    // PREFER CLAUDE_CODE_SESSION_ID: it is rename-immune and is the same id the CLI later
    // resolves from, so the handle is bound under the identical sid it will be looked up
    // by. A Conductor worktree rename mid-session changes the path-derived key but not
    // this.
    let sid = std::env::var("CLAUDE_CODE_SESSION_ID").ok()
        .filter(|s| !s.is_empty())
        .or_else(|| field(&p, "session_id").map(str::to_string))
        // Last resort: the transcript basename is stable per session.
        .or_else(|| field(&p, "transcript_path").and_then(|t| {
            std::path::Path::new(t).file_stem().map(|s| s.to_string_lossy().to_string())
        }))
        .unwrap_or_default();
    if sid.is_empty() {
        return 0;
    }

    let Some(args) = presence_argv(event, &sid, ppid) else { return 0 };
    // Best-effort and silent: presence bookkeeping must never surface as noise in the
    // session, and must never block it.
    let _ = std::process::Command::new(std::env::current_exe()
            .unwrap_or_else(|_| "paosctl".into()))
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    0
}

/// Long-term memory digest + the paos activation directive, at SessionStart.
///
/// FAIL-SAFE by construction: the directive always fires (it is the activation nudge), and
/// any recall failure simply drops the digest. A hook that blocks session start is worse
/// than one that says nothing.
const PREF_QUERY: &str = "standing user preferences, conventions, rules, how I like to work";
/// Lessons are phrased trigger -> failure -> fix, so the query names the SHAPE rather than
/// a topic. The preference query does not reach them: a trap is not a preference.
const LESSON_QUERY: &str = "lesson: a mistake that cost sessions time, what failed and the fix";
const TOP_K: usize = 3;
const LESSON_TOP_K: usize = 2;
/// Lean, but lessons earn their space. Full text stays recallable on demand.
const DIGEST_CAP: usize = 1600;

const DIRECTIVE: &str = concat!(
    "paos (Personal Agentic OS): activate the `paos` skill now. Memory: run ",
    "`~/.claude/skills/paos/paos memory recall <q>` before searching for anything you may ",
    "have established before, and `paos memory remember` durable facts mid-work. Bus: on ",
    "your first turn run `~/.claude/skills/paos/paos bus hello --task \"<task>\"` to join ",
    "the lobby and announce yourself, and keep a detached background `paos bus wait-joined` ",
    "listener armed at the end of every turn. Operator: if you're blocked and the operator ",
    "is away, escalate with `paos operator ask`.",
);

fn recall(query: &str, top_k: usize, timeout_secs: u64) -> Option<Vec<String>> {
    let exe = std::env::current_exe().ok()?;
    let mut child = std::process::Command::new(exe)
        .args(["recall", query, "--top-k", &top_k.to_string()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    // Bound the wait: a wedged memory backend must not stall session start. The Python
    // used a per-recall subprocess timeout for exactly this reason.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => break,
            Ok(Some(_)) => return None,
            Err(_) => return None,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                return None;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
    let out = child.wait_with_output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("- ").map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .collect())
}

pub fn build_digest(prefs: &[String], lessons: &[String], project: &[String], proj: &str)
    -> String
{
    let mut seen = std::collections::HashSet::new();
    let mut fresh = |facts: &[String]| -> Vec<String> {
        facts.iter()
            .filter(|f| {
                let k = f.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
                !k.is_empty() && seen.insert(k)
            })
            .cloned()
            .collect()
    };
    let (p, ls, pj) = (fresh(prefs), fresh(lessons), fresh(project));
    if p.is_empty() && ls.is_empty() && pj.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "## Long-term memory (paos) — surfaced at session start; judge relevance".to_string()];
    if !p.is_empty() {
        lines.push("**Standing preferences:**".into());
        lines.extend(p.iter().map(|f| format!("- {f}")));
    }
    // Lessons before project facts: they are traps that already cost several sessions real
    // time, and the TAIL of this digest is what gets cut when it runs long.
    if !ls.is_empty() {
        lines.push("**Learned the hard way (recurred across sessions):**".into());
        lines.extend(ls.iter().map(|f| format!("- {f}")));
    }
    if !pj.is_empty() {
        lines.push(format!("**This project ({}):**",
                           if proj.is_empty() { "current" } else { proj }));
        lines.extend(pj.iter().map(|f| format!("- {f}")));
    }
    // Drop whole LINES rather than slicing mid-sentence. A fact cut in half is worse than
    // one absent: the reader cannot tell it is incomplete, so a lesson can lose the very
    // clause carrying the fix.
    //
    // Evict the LOWEST-RANKED fact from whichever section currently has the most, rather
    // than popping the tail. Tail-popping starves whatever comes last, which on this
    // machine was the whole lessons section: three long standing preferences (466 + 419 +
    // 180 chars) nearly filled the cap, the first lesson crossed it, and the bullet was
    // popped while its header stayed — so every session read "Learned the hard way
    // (recurred across sessions):" with nothing beneath it. The tier that exists to stop a
    // session re-deriving a trap another session already paid for reached nobody, and the
    // comment above this loop had ALREADY tried to protect lessons by ordering them before
    // project facts. Ordering cannot help when the eviction rule is positional.
    let size = |ls: &Vec<String>| ls.iter().map(|l| l.len() + 1).sum::<usize>();
    while lines.len() > 1 && size(&lines) > DIGEST_CAP {
        // Bullet indices grouped by the header they sit under.
        let mut sections: Vec<Vec<usize>> = Vec::new();
        for (i, l) in lines.iter().enumerate() {
            if l.starts_with("**") {
                sections.push(Vec::new());
            } else if l.starts_with("- ") {
                if let Some(s) = sections.last_mut() {
                    s.push(i);
                }
            }
        }
        // The fullest section, ties broken toward the LAST one so a section that already
        // gave up a fact is not asked again while an equal-sized earlier one keeps all of
        // its own.
        let victim = sections
            .iter()
            .filter(|s| s.len() > 1)
            .max_by_key(|s| s.len())
            .or_else(|| sections.iter().filter(|s| !s.is_empty()).next_back())
            .and_then(|s| s.last().copied());
        match victim {
            Some(i) => { lines.remove(i); }
            // Nothing left to evict but still over cap: fall back to the tail so this
            // cannot loop forever.
            None => { lines.pop(); }
        }
        // A header whose bullets are all gone promises facts and shows none — strictly
        // worse than omitting the section, because the reader cannot tell whether the
        // tier is empty or was truncated.
        let mut keep = Vec::with_capacity(lines.len());
        for (i, l) in lines.iter().enumerate() {
            let orphan = l.starts_with("**")
                && !lines.get(i + 1).is_some_and(|n| n.starts_with("- "));
            if !orphan {
                keep.push(l.clone());
            }
        }
        lines = keep;
    }
    lines.join("\n")
}

fn project_name(cwd: &str) -> String {
    let top = std::process::Command::new("git")
        .args(["-C", cwd, "rev-parse", "--show-toplevel"])
        .output().ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let path = top.unwrap_or_else(|| cwd.trim_end_matches('/').to_string());
    std::path::Path::new(&path).file_name()
        .map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

fn paos_init() -> i32 {
    let p = payload();
    let mut digest = String::new();
    // Skipped on resume: the session already has its history.
    if field(&p, "source") != Some("resume") {
        let cwd = field(&p, "cwd").map(str::to_string)
            .or_else(|| std::env::current_dir().ok().map(|d| d.display().to_string()))
            .unwrap_or_default();
        // If the FIRST recall fails the backend is unreachable or wedged; skip the rest,
        // bounding worst-case session-start delay to one timeout instead of three.
        if let Some(prefs) = recall(PREF_QUERY, TOP_K, 4) {
            let proj = project_name(&cwd);
            let project = if proj.is_empty() { vec![] } else {
                recall(&format!("{proj} {cwd} — project state, decisions, setup, gotchas"),
                       TOP_K, 4).unwrap_or_default()
            };
            // Shorter timeout and a smaller k: this is the third recall, and session start
            // should not pay a third full stall for the least critical section.
            let lessons = recall(LESSON_QUERY, LESSON_TOP_K, 2).unwrap_or_default();
            digest = build_digest(&prefs, &lessons, &project, &proj);
        }
    }
    let ctx = if digest.is_empty() { DIRECTIVE.to_string() }
              else { format!("{digest}\n\n{DIRECTIVE}") };
    println!("{}", serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": ctx,
        }
    }));
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(json: &str) -> serde_json::Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn an_empty_field_is_treated_as_absent() {
        // A payload carrying "session_id": "" must fall through to the next source, not
        // bind the handle to an empty id.
        assert_eq!(field(&v(r#"{"session_id":""}"#), "session_id"), None);
        assert_eq!(field(&v(r#"{"session_id":"abc"}"#), "session_id"), Some("abc"));
    }

    #[test]
    fn a_missing_field_is_none_rather_than_a_panic() {
        assert_eq!(field(&v("{}"), "nope"), None);
        assert_eq!(field(&serde_json::Value::Null, "nope"), None);
    }

    #[test]
    fn the_transcript_basename_is_a_usable_session_id() {
        // The documented fallback when session_id is absent.
        let stem = std::path::Path::new("/x/y/0a060134-abcd.jsonl")
            .file_stem().unwrap().to_string_lossy().to_string();
        assert_eq!(stem, "0a060134-abcd");
    }

    fn v_of(xs: &[&str]) -> Vec<String> { xs.iter().map(|s| s.to_string()).collect() }

    #[test]
    fn lessons_get_their_own_section_and_reach_a_new_session() {
        // The whole point of mining them: a trap that cost several sessions time must
        // arrive BEFORE the next session repeats it.
        let d = build_digest(&v_of(&["p"]), &v_of(&["do not use shell backgrounding"]),
                             &v_of(&["proj"]), "dotfiles");
        assert!(d.contains("Learned the hard way"), "{d}");
        assert!(d.contains("do not use shell backgrounding"));
    }

    #[test]
    fn a_fact_is_not_repeated_across_two_sections() {
        // Recall queries overlap; the same fact surfacing twice wastes the cap.
        let d = build_digest(&v_of(&["same fact"]), &v_of(&["Same   Fact"]), &[], "x");
        assert_eq!(d.matches("same fact").count() + d.matches("Same   Fact").count(), 1, "{d}");
    }

    #[test]
    fn lessons_outrank_project_facts_when_space_is_tight() {
        // The digest is truncated from the TAIL, and a trap that already cost real time
        // is worth more than one more project note.
        let long = "x".repeat(900);
        let d = build_digest(&v_of(&["p"]), &v_of(&["the lesson"]), &v_of(&[&long]), "x");
        assert!(d.contains("the lesson"), "lessons must survive truncation");
    }

    #[test]
    fn truncation_never_cuts_a_fact_in_half() {
        // A fact cut mid-sentence is worse than one absent: the reader cannot tell it is
        // incomplete, so a lesson can lose the very clause carrying the fix.
        let facts = v_of(&["a".repeat(400).as_str(), "b".repeat(400).as_str(),
                           "c".repeat(400).as_str(), "d".repeat(400).as_str()]);
        let d = build_digest(&facts, &[], &[], "x");
        for line in d.lines().filter(|l| l.starts_with("- ")) {
            let body = &line[2..];
            assert!(facts.iter().any(|f| f == body), "a fact was truncated: {} chars", body.len());
        }
    }

    #[test]
    fn truncation_never_leaves_a_section_header_with_nothing_under_it() {
        // LIVE DEFECT, reproduced from the real machine on 2026-07-31: every session on
        // this machine started with
        //     **Learned the hard way (recurred across sessions):**
        // and NOTHING beneath it. Three long standing preferences (466 + 419 + 180 chars)
        // nearly fill DIGEST_CAP, so adding the first lesson crosses it, the loop pops the
        // bullet, and the header is left behind promising content that was just removed.
        //
        // The cost is not cosmetic: the lessons tier is the one that exists to stop a
        // session re-deriving a trap another session already paid for — and it reached
        // nobody. I re-derived one myself today that had been stored on 2026-07-27.
        let long = |n: usize, tag: &str| format!("- [2026-07-26 0.30] {tag} {}", "x".repeat(n));
        let prefs = vec![long(440, "pref one"), long(400, "pref two"), long(160, "pref three")];
        let lessons = vec![long(300, "the lesson that must not vanish silently")];
        let out = build_digest(&prefs, &lessons, &[], "proj");
        let lines: Vec<&str> = out.lines().collect();
        for (i, l) in lines.iter().enumerate() {
            if l.starts_with("**") {
                let has_content = lines.get(i + 1).is_some_and(|n| n.starts_with("- "));
                assert!(has_content,
                        "section header {l:?} has no bullets under it — truncation left an \
                         orphan, which tells the reader facts exist while showing none");
            }
        }
    }

    #[test]
    fn no_lessons_leaves_the_section_out_entirely() {
        let d = build_digest(&v_of(&["p"]), &[], &v_of(&["proj"]), "x");
        assert!(!d.contains("Learned the hard way"));
    }

    #[test]
    fn nothing_recalled_yields_an_empty_digest_not_an_empty_header() {
        // An empty header block would spend context saying nothing.
        assert_eq!(build_digest(&[], &[], &[], "x"), "");
    }

    #[test]
    fn a_whitespace_only_fact_produces_no_bullet() {
        assert_eq!(build_digest(&v_of(&["   "]), &[], &[], "x"), "");
    }

    // The argv is measured against the Python hook, not guessed. Reference, captured by
    // importing `session-presence` and stubbing `_run_paos`:
    //     session-start --session-id <sid> --ppid <ppid>
    //     heartbeat     --session-id <sid> --ppid <ppid>
    //     session-end   --session-id <sid>
    #[test]
    fn session_start_puts_the_ppid_in_the_pid_flag_not_the_handle_slot() {
        // The regression that mattered: a bare `4242` here became the session's HANDLE.
        assert_eq!(
            presence_argv("SessionStart", "sid-1", "4242").unwrap(),
            vec!["bus", "session-start", "--session-id", "sid-1", "--ppid", "4242"]
        );
    }

    #[test]
    fn stop_sends_a_heartbeat_and_session_end_sends_no_ppid() {
        assert_eq!(
            presence_argv("Stop", "sid-1", "4242").unwrap(),
            vec!["bus", "heartbeat", "--session-id", "sid-1", "--ppid", "4242"]
        );
        // Python's session-end takes no ppid either — an extra flag here would diverge.
        assert_eq!(
            presence_argv("SessionEnd", "sid-1", "4242").unwrap(),
            vec!["bus", "session-end", "--session-id", "sid-1"]
        );
    }

    #[test]
    fn every_value_is_preceded_by_its_own_flag() {
        // Structural guard rather than a literal: catches a value sliding into the
        // positionals — the shape of the bug — even if the verbs are renamed later.
        for ev in ["SessionStart", "Stop", "SessionEnd"] {
            let a = presence_argv(ev, "sid-1", "4242").unwrap();
            assert_eq!(a[0], "bus");
            let mut i = 2;
            while i < a.len() {
                assert!(a[i].starts_with("--"), "{ev}: {:?} is a bare positional in {a:?}", a[i]);
                assert!(i + 1 < a.len(), "{ev}: flag {:?} has no value in {a:?}", a[i]);
                i += 2;
            }
        }
    }

    #[test]
    fn an_unhandled_event_produces_no_command_at_all() {
        // PreCompact, Notification, etc. must not reach the bus.
        assert!(presence_argv("PreCompact", "sid-1", "4242").is_none());
        assert!(presence_argv("", "sid-1", "4242").is_none());
    }

    #[test]
    fn an_unknown_hook_name_still_exits_zero() {
        // A hook that fails a session is worse than a hook that does nothing.
        assert_eq!(run(&["hook".into(), "nonsense".into()], &[]), 0);
    }
}
