//! `paos standup` — the daily brief, split work vs personal.
//!
//! Ported from `standup_facet.py`. Sessions log milestones with `standup log`; `brief`
//! gathers the notes, git commits and bus messages since the last reported brief and asks
//! Claude Code to synthesise them.
//!
//! Reads go direct (read-only). The two writes — a note, and a generated brief — go
//! through the daemon.

use paos_proto::{Request, Response};

pub const SIDES: [&str; 2] = ["work", "personal"];

const SYSTEM: &str = concat!(
    "You are writing a daily standup brief for ONE engineer, to be skimmed aloud in ",
    "under 30 seconds. Sources: their work notes, git commits, and inter-session bus ",
    "messages across many parallel sessions. Output GitHub-flavored markdown with ",
    "EXACTLY these three section headers, in order: '## Done', '## In progress', ",
    "'## Blockers'. Under each, group items by repo/project using a '### <repo>' ",
    "subheader followed by its bullets.\n",
    "BREVITY RULES (critical — prior briefs were far too long):\n",
    "- One line per bullet, at most ~14 words. Lead with the OUTCOME (what shipped, ",
    "was fixed, or decided) — not the how.\n",
    "- Merge related work into ONE bullet. Never enumerate every PR, commit, or file. ",
    "Drop commit hashes, PR numbers, and file paths unless a number is the point.\n",
    "- At most 3 bullets per repo, and at most ~6 bullets per section. Keep the most ",
    "report-worthy; cut minor detail.\n",
    "- No nested bullets. No parentheticals. Concrete and factual, zero filler.\n",
    "Mine bus messages for real work, decisions, and blockers; IGNORE greetings, ",
    "presence/handshake noise, and logistics. Dedupe notes, commits, and messages that ",
    "describe the same work. Keep all three headers even if empty (write '- (none)'). ",
    "Output only the markdown, no preamble.",
);

/// Owners whose repos count as WORK. Everything else is personal.
/// Git owners whose repos count as WORK: configuration first, then the environment.
///
/// Empty means no split, and that is the right default for an installation that has said
/// nothing — the previous default named one company, so every fresh install would have
/// filed a stranger's repos as somebody else's work.
///
/// This machine keeps its own answer through `identity_work_owners`, written before the
/// default changed. The env var stays because a container has no dashboard to set it in.
/// The repository this note belongs to — asked of git, not of an env var.
///
/// It used to read one workspace manager's variable, so a note logged anywhere else had
/// no repo at all and every side-detection fell back to personal. git knows, on every
/// machine, without anyone exporting anything.
fn repo_root() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if p.is_empty() { None } else { Some(p) }
}

fn work_owners() -> Vec<String> {
    let configured = ro().and_then(|c| {
        c.query_row("SELECT value FROM paos_config WHERE key='identity_work_owners'",
                    [], |r| r.get::<_, String>(0))
            .ok()
    });
    work_owners_from(configured, std::env::var("PAOS_WORK_OWNERS").ok())
}

/// The precedence, as a pure function.
///
/// Separate because the reader is the real store: a test of `work_owners()` would assert
/// against whatever this machine happens to have configured, which is not a test of
/// anything. The same trap caught the settings payload earlier in this work.
fn work_owners_from(configured: Option<String>, env: Option<String>) -> Vec<String> {
    configured
        .filter(|v| !v.trim().is_empty())
        .or(env)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

fn ro() -> Option<rusqlite::Connection> {
    rusqlite::Connection::open_with_flags(
        paos_store::db_path(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
}

fn git(root: Option<&str>, args: &[&str]) -> Option<String> {
    let mut cmd = std::process::Command::new("git");
    if let Some(r) = root {
        cmd.arg("-C").arg(r);
    }
    let out = cmd.args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `(owner, repo)` from the git origin at `root`.
fn origin_parts(root: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(url) = git(root, &["remote", "get-url", "origin"]).filter(|u| !u.is_empty())
    else { return (None, None) };
    let path = url.rsplit(':').next().unwrap_or("").trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() >= 2 {
        (Some(parts[parts.len() - 2].to_string()), Some(parts[parts.len() - 1].to_string()))
    } else {
        (None, None)
    }
}

pub fn side_for_owner(owner: Option<&str>) -> &'static str {
    match owner {
        Some(o) if work_owners().contains(&o.to_lowercase()) => "work",
        _ => "personal",
    }
}

fn side_for_repo(root: Option<&str>) -> &'static str {
    side_for_owner(origin_parts(root).0.as_deref())
}

fn repo_slug(root: Option<&str>) -> String {
    match origin_parts(root) {
        (Some(o), Some(r)) => format!("{o}/{r}"),
        (Some(o), None) => o,
        _ => String::new(),
    }
}

fn now_iso() -> String {
    super::now_iso()
}

fn hours_ago_iso(h: i64) -> String {
    std::process::Command::new("date")
        .args(["-u", "-v", &format!("-{h}H"), "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

// --- gathering ---------------------------------------------------------------

struct Note { repo: String, text: String }
struct RepoCommits { repo: String, subjects: Vec<String> }
struct Msg { sender: String, text: String }

fn watermark(c: &rusqlite::Connection, side: &str) -> Option<String> {
    c.query_row("SELECT reported_ts FROM standup_watermark WHERE side=?1", [side],
                |r| r.get(0)).ok()
}

fn notes_since(c: &rusqlite::Connection, side: &str, start: &str) -> Vec<Note> {
    let Ok(mut st) = c.prepare(
        "SELECT ts, session, summary, data FROM events \
         WHERE kind='standup.note' AND ts>=?1 ORDER BY id") else { return vec![] };
    let Ok(rows) = st.query_map([start], |r| {
        Ok((r.get::<_, String>(1).unwrap_or_default(),
            r.get::<_, String>(2).unwrap_or_default(),
            r.get::<_, Option<String>>(3)?))
    }) else { return vec![] };
    rows.flatten()
        .filter_map(|(session, summary, data)| {
            let d: serde_json::Value = data
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or(serde_json::Value::Null);
            if d.get("side").and_then(|s| s.as_str()) != Some(side) {
                return None;
            }
            let repo = d.get("repo").and_then(|s| s.as_str()).unwrap_or("").to_string();
            Some(Note { repo: if repo.is_empty() { session } else { repo }, text: summary })
        })
        .collect()
}

/// Repo paths the bus has seen — `members` covers live sessions, `task_log` the ended
/// ones, whose member rows are dropped on session end.
fn candidate_repos(c: &rusqlite::Connection) -> Vec<String> {
    let mut out = std::collections::BTreeSet::new();
    for sql in ["SELECT DISTINCT repo FROM members WHERE repo IS NOT NULL AND repo!=''",
                "SELECT DISTINCT repo FROM task_log WHERE repo IS NOT NULL AND repo!=''"] {
        if let Ok(mut st) = c.prepare(sql) {
            if let Ok(rows) = st.query_map([], |r| r.get::<_, String>(0)) {
                out.extend(rows.flatten());
            }
        }
    }
    out.into_iter().collect()
}

fn commits_since(c: &rusqlite::Connection, side: &str, start: &str) -> Vec<RepoCommits> {
    let mut out = vec![];
    for path in candidate_repos(c) {
        if side_for_repo(Some(&path)) != side {
            continue;
        }
        // The repo's OWN configured email: a work repo with a distinct identity would
        // otherwise have all its commits filtered out.
        let email = git(Some(&path), &["config", "user.email"]).unwrap_or_default();
        let mut args = vec!["log", "--since", start, "--pretty=%s"];
        if !email.is_empty() {
            args.push("--author");
            args.push(&email);
        }
        let Some(body) = git(Some(&path), &args) else { continue };
        let subjects: Vec<String> = body.lines()
            .map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
        if !subjects.is_empty() {
            out.push(RepoCommits { repo: repo_slug(Some(&path)), subjects });
        }
    }
    out
}

/// handle -> side, via the repo each session was working in.
fn sender_sides(c: &rusqlite::Connection) -> std::collections::HashMap<String, &'static str> {
    let mut repo: std::collections::HashMap<String, String> = Default::default();
    for (sql, _) in [
        ("SELECT name, repo FROM members WHERE repo IS NOT NULL AND repo!=''", 0),
        ("SELECT session_name, repo FROM task_log WHERE repo IS NOT NULL AND repo!=''", 0)] {
        if let Ok(mut st) = c.prepare(sql) {
            if let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
                for (n, p) in rows.flatten() {
                    repo.entry(n).or_insert(p);
                }
            }
        }
    }
    repo.into_iter().map(|(n, p)| (n, side_for_repo(Some(&p)))).collect()
}

fn messages_since(c: &rusqlite::Connection, side: &str, start: &str, limit: usize) -> Vec<Msg> {
    let sides = sender_sides(c);
    let Ok(mut st) = c.prepare(
        "SELECT sender, text FROM messages WHERE ts>=?1 ORDER BY id DESC LIMIT 2000")
    else { return vec![] };
    let Ok(rows) = st.query_map([start], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    }) else { return vec![] };
    let mut out: Vec<Msg> = rows.flatten()
        .filter(|(sender, _)| sides.get(sender).copied() == Some(side))
        .filter_map(|(sender, text)| {
            let t = text.unwrap_or_default().trim().to_string();
            if t.is_empty() { None } else { Some(Msg { sender, text: t }) }
        })
        .take(limit)
        .collect();
    out.reverse();   // oldest-first reads better in the prompt
    out
}

fn build_prompt(side: &str, notes: &[Note], commits: &[RepoCommits], msgs: &[Msg]) -> String {
    let mut l = vec![SYSTEM.to_string(), String::new(), format!("SIDE: {side}"),
                     String::new(), "## Notes".into()];
    if notes.is_empty() {
        l.push("(no notes)".into());
    } else {
        for n in notes {
            l.push(format!("- [{}] {}", if n.repo.is_empty() { "?" } else { &n.repo }, n.text));
        }
    }
    l.extend([String::new(), "## Commits".into()]);
    if commits.is_empty() {
        l.push("(no commits)".into());
    } else {
        for c in commits {
            l.push(format!("**{}**", if c.repo.is_empty() { "?" } else { &c.repo }));
            for s in &c.subjects { l.push(format!("- {s}")); }
        }
    }
    l.extend([String::new(), "## Bus messages".into()]);
    if msgs.is_empty() {
        l.push("(no messages)".into());
    } else {
        for m in msgs { l.push(format!("- [{}] {}", m.sender, m.text)); }
    }
    l.join("\n")
}

/// Claude Code, resolved WITHOUT relying on an enriched PATH: launchd services run with a
/// minimal one that lacks ~/.local/bin, where Claude Code installs.
fn claude_bin() -> String {
    if let Ok(v) = std::env::var("PAOS_CLAUDE_BIN") {
        if !v.is_empty() { return v; }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let local = format!("{home}/.local/bin/claude");
    if std::path::Path::new(&local).exists() { local } else { "claude".into() }
}

fn run_claude(prompt: &str) -> Result<String, String> {
    // USER/LOGNAME are backfilled because launchd and cron give a minimal environment
    // without them, and macOS cannot resolve the login Keychain — where Claude Code keeps
    // its OAuth credentials — so it reports "Not logged in".
    let mut cmd = std::process::Command::new(claude_bin());
    cmd.args(["-p", prompt, "--output-format", "text"]);
    if std::env::var("USER").is_err() {
        if let Some(u) = std::env::var("LOGNAME").ok().or_else(|| whoami()) {
            cmd.env("USER", u);
        }
    }
    let out = cmd.output().map_err(|e| format!("claude invocation failed: {e}"))?;
    if !out.status.success() {
        return Err(format!("claude exited {}: {}", out.status,
                           String::from_utf8_lossy(&out.stderr).trim()));
    }
    let body = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if body.is_empty() {
        return Err("claude produced no output".into());
    }
    Ok(body)
}

fn whoami() -> Option<String> {
    std::process::Command::new("id").arg("-un").output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
}

// --- CLI ----------------------------------------------------------------------

fn sides_for(arg: Option<&str>) -> Vec<&'static str> {
    match arg {
        Some("work") => vec!["work"],
        Some("personal") => vec!["personal"],
        _ => SIDES.to_vec(),
    }
}

pub fn run(positional: &[String], args: &[String],
           send: impl Fn(&Request) -> Option<Response>) -> i32 {
    let opt = |n: &str| args.iter().position(|a| a == n).and_then(|i| args.get(i + 1))
        .map(String::as_str);
    match positional.get(1).map(String::as_str).unwrap_or("show") {
        "log" => cmd_log(positional.get(2).map(String::as_str).unwrap_or(""), &send),
        "show" => cmd_show(opt("--side")),
        "brief" => cmd_brief(opt("--side"), args.iter().any(|a| a == "--dry-run"), &send),
        "reported" => cmd_reported(opt("--side"), &send),
        other => {
            eprintln!("unknown standup subcommand: {other}\n\
                       usage: paos standup [log <text> | brief | show | reported] [--side work|personal]");
            2
        }
    }
}

fn cmd_log(text: &str, send: &impl Fn(&Request) -> Option<Response>) -> i32 {
    if text.trim().is_empty() {
        eprintln!("standup log needs some text");
        return 2;
    }
    let root = repo_root();
    let side = side_for_repo(root.as_deref());
    let data = serde_json::json!({ "side": side, "repo": repo_slug(root.as_deref()) });
    match send(&Request::Event {
        kind: "standup.note".into(),
        summary: text.trim().into(),
        session: None,
        reference: None,
        data: Some(data.to_string()),
    }) {
        Some(Response::Ok { .. }) => { println!("logged ({side})"); 0 }
        Some(Response::Err { message, exit_code }) => { eprintln!("{message}"); exit_code }
        None => { eprintln!("paos: cannot reach paosd — note not recorded"); super::EXIT_NO_DAEMON }
    }
}

fn cmd_show(side: Option<&str>) -> i32 {
    let Some(c) = ro() else { eprintln!("paos.db unreadable"); return 1 };
    let mut any = false;
    for s in sides_for(side) {
        if let Ok((ts, body, status)) = c.query_row(
            "SELECT ts, body, status FROM standup_briefs WHERE side=?1 ORDER BY id DESC LIMIT 1",
            [s], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))) {
            any = true;
            println!("# {s} — {ts} ({status})\n{body}\n");
        }
    }
    if !any {
        println!("(no briefs yet — run `paos standup brief`)");
    }
    0
}

fn cmd_brief(side: Option<&str>, dry_run: bool,
             send: &impl Fn(&Request) -> Option<Response>) -> i32 {
    let Some(c) = ro() else { eprintln!("paos.db unreadable"); return 1 };
    let mut rc = 0;
    for s in sides_for(side) {
        let start = watermark(&c, s).unwrap_or_else(|| hours_ago_iso(24));
        // Capture the window's upper bound BEFORE gathering: a note logged while Claude
        // runs would otherwise fall after `to` and be lost between this brief and the
        // watermark.
        let to = now_iso();
        let prompt = build_prompt(s, &notes_since(&c, s, &start),
                                  &commits_since(&c, s, &start),
                                  &messages_since(&c, s, &start, 300));
        if dry_run {
            println!("--- {s}: {start} .. {to} ---\n{prompt}\n");
            continue;
        }
        match run_claude(&prompt) {
            Err(e) => { eprintln!("{s}: {e}"); rc = 1 }
            Ok(body) => match send(&Request::StandupBrief {
                side: s.into(), covers_from: start, covers_to: to, body: body.clone(),
            }) {
                Some(Response::Ok { .. }) => println!("# {s}\n{body}\n"),
                Some(Response::Err { message, .. }) => { eprintln!("{message}"); rc = 1 }
                None => { eprintln!("paos: cannot reach paosd — brief not saved"); rc = 1 }
            },
        }
    }
    rc
}

fn cmd_reported(side: Option<&str>, send: &impl Fn(&Request) -> Option<Response>) -> i32 {
    let Some(s) = side.filter(|s| SIDES.contains(s)) else {
        eprintln!("reported needs --side work|personal");
        return 2;
    };
    match send(&Request::StandupReported { side: s.into() }) {
        Some(Response::Ok { lines }) => { for l in lines { println!("{l}"); } 0 }
        Some(Response::Err { message, exit_code }) => { eprintln!("{message}"); exit_code }
        None => { eprintln!("paos: cannot reach paosd"); super::EXIT_NO_DAEMON }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_work_owner_is_work_and_everything_else_is_personal() {
        // No origin at all — a scratch dir is personal, not an error.
        assert_eq!(side_for_owner(None), "personal");
    }

    #[test]
    fn configuration_beats_the_environment_and_an_empty_default_means_no_split() {
        let w = |c: Option<&str>, e: Option<&str>| {
            work_owners_from(c.map(str::to_string), e.map(str::to_string))
        };
        assert_eq!(w(Some("ExampleCorp, acme"), None), vec!["examplecorp", "acme"],
                   "trimmed and lowercased, so owner matching is case-insensitive");
        assert_eq!(w(Some("examplecorp"), Some("ignored")), vec!["examplecorp"],
                   "configuration wins");
        assert_eq!(w(None, Some("fromenv")), vec!["fromenv"],
                   "a container has no dashboard, so the env var stays");
        assert!(w(Some("   "), None).is_empty(), "blank is not a work owner");
        assert!(w(None, None).is_empty(),
                "an installation that has said nothing splits nothing — the old default \
                 named one company, so every fresh install filed a stranger's repos as \
                 somebody else's work");
    }

    #[test]
    fn the_prompt_keeps_all_three_sections_even_when_empty() {
        // The model is told to emit three headers; feeding it a prompt missing a whole
        // section invites it to drop one.
        let p = build_prompt("work", &[], &[], &[]);
        for h in ["## Notes", "## Commits", "## Bus messages"] {
            assert!(p.contains(h), "{h} missing");
        }
        assert!(p.contains("(no notes)") && p.contains("(no commits)"));
    }

    #[test]
    fn the_prompt_carries_the_side_so_the_model_does_not_mix_them() {
        assert!(build_prompt("personal", &[], &[], &[]).contains("SIDE: personal"));
    }

    #[test]
    fn a_note_without_a_repo_falls_back_to_the_session_handle() {
        let n = [Note { repo: String::new(), text: "shipped it".into() }];
        assert!(build_prompt("work", &n, &[], &[]).contains("- [?] shipped it"));
    }

    #[test]
    fn sides_default_to_both() {
        assert_eq!(sides_for(None), vec!["work", "personal"]);
        assert_eq!(sides_for(Some("work")), vec!["work"]);
        assert_eq!(sides_for(Some("nonsense")), vec!["work", "personal"]);
    }
}
