//! `paos selftest` — EXERCISE the paths a session depends on, from where a session stands.
//!
//! `doctor` inspects STATE: is the store being written, is the bridge polling, is there a
//! backup. Necessary and not sufficient. Three failures in one day were green on doctor
//! throughout, because state looked fine while the path was broken: memory writes were
//! dead fleet-wide while the fact count looked healthy, the spool drained 60x slower than
//! the CLI promised, and `doctor` itself did not exist as a facet.
//!
//! The difference is doing versus looking. This writes a real fact, waits for it, reads it
//! back and deletes it. It runs through this same binary, in whatever sandbox the caller
//! is in, because a test that only passes from an unsandboxed terminal proves nothing
//! about this machine's actual users.

use std::process::Command;

/// Three states, not two.
///
/// "I could not check" is not "nothing is wrong", and collapsing them is how the Python
/// version's own first run printed a green doctor while doctor could not start at all —
/// the output had no failure lines to find, so "could not check" scored as "fine".
#[derive(PartialEq, Clone, Copy)]
enum State { Ok, Fail, Unknown }

impl State {
    fn mark(self) -> &'static str {
        match self { State::Ok => "✓", State::Fail => "✗", State::Unknown => "?" }
    }
}

struct Report { rows: Vec<(String, State)> }

impl Report {
    fn new() -> Self { Report { rows: vec![] } }
    fn add(&mut self, name: &str, state: State, detail: &str) {
        println!("  {} {name:<26} {detail}", state.mark());
        self.rows.push((name.to_string(), state));
    }
    fn count(&self, s: State) -> usize { self.rows.iter().filter(|r| r.1 == s).count() }
}

/// Invoke ourselves the way a session would.
fn me() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "paos".into())
}

fn run(args: &[&str]) -> (i32, String) {
    match Command::new(me()).args(args).output() {
        Ok(o) => (o.status.code().unwrap_or(-1),
                  String::from_utf8_lossy(&o.stdout).to_string()
                      + &String::from_utf8_lossy(&o.stderr)),
        Err(e) => (1, e.to_string()),
    }
}

/// Run doctor ONCE and say honestly whether it produced a verdict.
///
/// From inside a sandbox `doctor` needs the socket and exits without check lines at all.
/// Scanning that output for failures finds none, which looks exactly like a clean bill of
/// health.
fn read_doctor() -> (String, bool) {
    let (rc, out) = run(&["doctor"]);
    let reachable = (rc == 0 || rc == 1) && (out.contains('✓') || out.contains('✗'));
    (out, reachable)
}

pub fn run_selftest(keep: bool) -> i32 {
    println!("paos selftest — exercising the paths a session uses\n");
    let mut rep = Report::new();
    let (doc, reachable) = read_doctor();

    if reachable {
        // Host pressure is a real alert but not a broken capability; this is about paths.
        let fails: Vec<&str> = doc.lines()
            .map(str::trim)
            .filter(|l| l.starts_with('✗'))
            .filter(|l| !l.contains("swap") && !l.contains("disk"))
            .collect();
        if fails.is_empty() {
            rep.add("doctor", State::Ok, "no capability failures");
        } else {
            rep.add("doctor", State::Fail, &fails.join("; "));
        }
    } else {
        rep.add("doctor", State::Unknown, "doctor could not run (socket blocked) — NOT a pass");
    }

    check_bus(&mut rep);

    if reachable {
        let live = doc.contains("✓ telegram");
        rep.add("operator path", if live { State::Ok } else { State::Fail },
                if live { "Telegram bridge live" }
                else { "bridge NOT reaching Telegram — the operator cannot be told anything" });
    } else {
        rep.add("operator path", State::Unknown,
                "cannot see the socket from here — re-run outside the sandbox to check");
    }

    check_memory(&mut rep, keep);
    check_tasks(&mut rep, keep);

    let (bad, unknown) = (rep.count(State::Fail), rep.count(State::Unknown));
    println!();
    if bad > 0 {
        println!("{bad} of {} checks FAILED — a session relying on these would lose work",
                 rep.rows.len());
        return 1;
    }
    if unknown > 0 {
        println!("{} of {} checks passed; {unknown} could not be determined from here.\n\
                  Re-run outside the agent sandbox to check those.",
                 rep.rows.len() - unknown, rep.rows.len());
        return 0;
    }
    println!("all {} checks passed — write, read, bus and operator paths all work from here",
             rep.rows.len());
    0
}

fn check_bus(rep: &mut Report) {
    let (rc, out) = run(&["bus", "whoami"]);
    if rc != 0 || out.trim().is_empty() {
        rep.add("bus identity", State::Fail, &format!("whoami exited {rc}"));
        return;
    }
    rep.add("bus identity", State::Ok, out.trim().lines().last().unwrap_or("").trim());

    // exit 1 means "not listening", a real state a session must fix — not a crash.
    let (rc, _) = run(&["bus", "reachable"]);
    rep.add("bus reachable", if rc == 0 { State::Ok } else { State::Fail },
            if rc == 0 { "rooms ok, listener live" }
            else { "NOT LISTENING — arm `paos bus wait-joined`" });
}

/// The one that matters most: can a session store a fact and get it back?
///
/// The marker is unique so the recall cannot pass on a pre-existing fact, which is how a
/// broken write path can look healthy.
fn check_memory(rep: &mut Report, keep: bool) {
    let marker = format!("paos-selftest-{}-{}",
                         std::process::id(),
                         std::time::SystemTime::now()
                             .duration_since(std::time::UNIX_EPOCH)
                             .map(|d| d.as_millis()).unwrap_or(0));
    let text = format!("{marker} — transient selftest fact, safe to delete");
    let (rc, out) = run(&["memory", "remember", "--project", &text]);
    if rc != 0 {
        rep.add("memory write", State::Fail,
                &format!("remember exited {rc}: {}", out.trim()));
        return;
    }
    let spooled = out.contains("spooled");
    rep.add("memory write", State::Ok,
            if spooled { "spooled, awaiting the daemon" } else { "stored directly" });

    // Poll rather than sleep a fixed time: the spool drains every 5s, and a fixed wait
    // either wastes time or reports a false failure on a slow machine.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut found = false;
    let mut last = String::new();
    while std::time::Instant::now() < deadline {
        let (_, out) = run(&["memory", "recall", &marker]);
        last = out;
        if last.contains(&marker) { found = true; break; }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    rep.add("memory recall", if found { State::Ok } else { State::Fail },
            if found { "read back" }
            else { "WROTE BUT COULD NOT READ BACK within 30s — it may exist as a spool file" });

    if last.contains("DEGRADED") {
        rep.add("recall quality", State::Ok,
                "degraded word-match (sandbox blocks the socket) — expected inside an agent");
    }
    if found && !keep {
        cleanup(rep, &marker);
    }
}

/// The work queue, end to end from where sessions actually run.
///
/// Worth its own leg for the same reason memory has one: the whole facet depends on a
/// spooled write being drained by the daemon, and that path has now twice been broken in
/// a way that reports success at the CLI. A unit test cannot see it — only a real write
/// followed by a real read-back can.
fn check_tasks(rep: &mut Report, keep: bool) {
    let marker = format!("paos-selftest-task-{}-{}",
                         std::process::id(),
                         std::time::SystemTime::now()
                             .duration_since(std::time::UNIX_EPOCH)
                             .map(|d| d.as_millis()).unwrap_or(0));
    let (rc, out) = run(&["task", "create", &marker, "--global"]);
    if rc != 0 {
        rep.add("task write", State::Fail, &format!("create exited {rc}: {}", out.trim()));
        return;
    }
    rep.add("task write", State::Ok,
            if out.contains("spooled") { "spooled, awaiting the daemon" } else { "stored directly" });

    // Poll for the row rather than sleeping: the spool drains every ~5s.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut id = String::new();
    while std::time::Instant::now() < deadline {
        let (_, listed) = run(&["task", "list", "--all"]);
        if let Some(line) = listed.lines().find(|l| l.contains(&marker)) {
            id = line.split_whitespace().next().unwrap_or("").to_string();
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    if id.is_empty() {
        rep.add("task read-back", State::Fail,
                "CREATED BUT NEVER APPEARED within 30s — the spool drain is not applying \
                 task ops, and every `paos task` write from a session is being lost");
        return;
    }
    rep.add("task read-back", State::Ok, &format!("landed as {id}"));

    // `ready` is the query sessions actually use to find work; a task that exists but
    // never surfaces there is invisible in the only place anyone looks.
    let (_, ready) = run(&["task", "ready", "--all"]);
    rep.add("task ready", if ready.contains(&id) { State::Ok } else { State::Fail },
            if ready.contains(&id) { "the new task is claimable" }
            else { "the task exists but does NOT show up in `ready`" });

    if !keep {
        let (rc, out) = run(&["task", "drop", &id]);
        rep.add("task cleanup", if rc == 0 { State::Ok } else { State::Fail },
                if rc == 0 { "dropped" } else { out.trim() });
    }
}

/// A test that litters the operator's memory is a bad test.
fn cleanup(rep: &mut Report, marker: &str) {
    let (_, datasets) = run(&["memory", "list"]);
    for line in datasets.lines() {
        let Some(ds) = line.split_whitespace().next().filter(|d| d.starts_with(|c: char| {
            c.is_alphanumeric()
        })) else { continue };
        let (_, items) = run(&["memory", "show", ds]);
        for item in items.lines() {
            if item.contains(marker) {
                if let Some(id) = item.split('[').nth(1).and_then(|s| s.split(']').next()) {
                    let (rc, o) = run(&["memory", "forget", id, "--force"]);
                    rep.add("cleanup", if rc == 0 { State::Ok } else { State::Fail },
                            if o.contains("spooled") { "spooled the delete" }
                            else { "removed the test fact" });
                    return;
                }
            }
        }
    }
    rep.add("cleanup", State::Unknown,
            &format!("could not locate the test fact to delete — search for {marker}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_is_not_counted_as_a_pass_or_a_failure() {
        // The distinction the Python version got wrong on its own first run.
        let mut r = Report::new();
        r.add("a", State::Ok, "");
        r.add("b", State::Unknown, "");
        assert_eq!(r.count(State::Ok), 1);
        assert_eq!(r.count(State::Fail), 0);
        assert_eq!(r.count(State::Unknown), 1);
    }

    #[test]
    fn each_state_has_a_distinct_mark() {
        // "?" reading as "✓" at a glance is the entire failure being guarded against.
        assert_ne!(State::Ok.mark(), State::Unknown.mark());
        assert_ne!(State::Fail.mark(), State::Unknown.mark());
    }
}
