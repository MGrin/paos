//! The five librarian passes: `draft`, `dream`, `lessons`, `tidy`, `split`.
//!
//! All the judgement lives in `paos-librarian`; this is the wiring — read the store
//! read-only, call the backend, queue proposals through the daemon.
//!
//! Every one of these REPORTS ITS FUNNEL rather than just a count. That is not cosmetic:
//! "0 proposals" hides three different situations — nothing durable happened, the model
//! never answered, and a guard vetoed everything — and the librarian being quietly broken
//! went unnoticed for weeks precisely because they looked the same from outside.

use paos_librarian as lib;
use paos_proto::{Request, Response};

fn value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn num(args: &[String], name: &str, default: usize) -> usize {
    value(args, name).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn read_only(db: &std::path::Path) -> Option<rusqlite::Connection> {
    rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
}

/// Which backend the passes will use. Config first (the dashboard writes it), then env.
fn backend(db: &std::path::Path) -> String {
    let configured = read_only(db).and_then(|c| lib::llm::configured_backend(&c));
    lib::llm::resolve_backend(configured.as_deref())
}

/// The dataset a scope resolves to for THIS cwd.
///
/// Python unpacked a (name, scope) tuple here and once passed the TUPLE through, which
/// made tidy find zero facts and report "nothing to merge" — a clean bill of health that
/// was really a type error.
fn scope_dataset(db: &std::path::Path, scope: Option<&str>) -> String {
    let origin = crate::git_origin().and_then(|o| paos_memory::scope::parse_origin(&o));
    match (scope.unwrap_or("project"), origin.as_ref()) {
        ("project", Some(o)) => paos_memory::scope::project_dataset(o),
        ("org", Some(o)) => paos_memory::scope::org_dataset(o),
        _ => configured_global(db),
    }
}

/// The global dataset this machine actually uses.
///
/// Read-only and failure-tolerant: a proposal drafted into the compiled-in default on a
/// machine that configured its own would be APPROVED into a dataset nothing ever recalls
/// from — a silent loss that looks like a successful write.
fn configured_global(db: &std::path::Path) -> String {
    rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .ok()
        .map(|c| paos_memory::scope::global_dataset(&c))
        .unwrap_or_else(|| paos_memory::scope::DEFAULT_GLOBAL.to_string())
}

fn scope_of(args: &[String]) -> Option<String> {
    for (f, s) in [("--global", "global"), ("--org", "org"), ("--project", "project")] {
        if args.iter().any(|a| a == f) {
            return Some(s.into());
        }
    }
    None
}

/// Queue one proposal through the daemon. Returns the printed reply line.
fn queue<F>(send: &F, kind: &str, dataset: &str, text: Option<&str>, scope: Option<&str>,
            target: Option<&str>, rationale: Option<&str>, source: &str) -> Option<String>
where
    F: Fn(&Request) -> Option<Response>,
{
    let req = Request::ProposalAdd {
        kind: kind.into(),
        dataset: dataset.into(),
        text: text.map(str::to_string),
        scope: scope.map(str::to_string),
        target_data_id: target.map(str::to_string),
        rationale: rationale.map(str::to_string),
        source: Some(source.into()),
    };
    match send(&req) {
        Some(Response::Ok { lines }) => Some(lines.join(" ")),
        Some(Response::Err { message, .. }) => {
            eprintln!("  queueing failed: {message}");
            None
        }
        None => {
            eprintln!("  paosd unreachable and the spool failed — proposal NOT queued");
            None
        }
    }
}

/// Live facts in one dataset, longest first — those are the entries most likely to be
/// several facts welded into one.
fn facts(db: &std::path::Path, dataset: &str, limit: usize, min_chars: Option<usize>)
    -> Vec<lib::upkeep::Fact>
{
    let Some(c) = read_only(db) else { return Vec::new() };
    let sql = match min_chars {
        Some(_) => "SELECT id, text FROM memories WHERE dataset=?1 AND superseded IS NULL \
                    AND length(text) > ?2 ORDER BY length(text) DESC LIMIT ?3",
        None => "SELECT id, text FROM memories WHERE dataset=?1 AND superseded IS NULL \
                 ORDER BY LENGTH(text) DESC LIMIT ?2",
    };
    let out = c.prepare(sql).and_then(|mut st| {
        let map = |r: &rusqlite::Row| {
            Ok(lib::upkeep::Fact { id: r.get(0)?, text: r.get(1)? })
        };
        match min_chars {
            Some(m) => st
                .query_map(rusqlite::params![dataset, m as i64, limit as i64], map)
                .and_then(|it| it.collect()),
            None => st
                .query_map(rusqlite::params![dataset, limit as i64], map)
                .and_then(|it| it.collect()),
        }
    });
    out.unwrap_or_default()
}

pub fn run<F>(cmd: &str, positional: &[String], args: &[String], db: &std::path::Path,
              send: F) -> i32
where
    F: Fn(&Request) -> Option<Response>,
{
    match cmd {
        "tidy" => cmd_tidy(args, db, &send),
        "split" => cmd_split(args, db, &send),
        "draft" => cmd_draft(positional, args, db, &send),
        "lessons" => cmd_lessons(args, db, &send),
        "dream" => cmd_dream(args, db, &send),
        _ => 2,
    }
}

fn cmd_tidy<F>(args: &[String], db: &std::path::Path, send: &F) -> i32
where
    F: Fn(&Request) -> Option<Response>,
{
    let dry = flag(args, "--dry-run");
    let ds = value(args, "--dataset").unwrap_or_else(|| scope_dataset(db, scope_of(args).as_deref()));
    let fs = facts(db, &ds, num(args, "--limit", 60), None);
    let groups = lib::upkeep::tidy_groups(&fs, 12);
    println!("{ds}: {} fact(s), {} group(s){}", fs.len(), groups.len(),
             if dry { " [dry-run]" } else { "" });
    if fs.is_empty() {
        // "Nothing to merge" and "I read nothing" are different answers.
        println!("  no facts in this dataset — nothing was read");
        return 0;
    }
    let b = backend(db);
    let (mut queued, mut unread) = (0usize, 0usize);
    for g in &groups {
        let Some(raw) = lib::draft::complete(lib::prompts::TIDY_SYS, &lib::upkeep::numbered(g), &b)
        else {
            unread += 1;
            continue;
        };
        for m in lib::upkeep::plan_merges(&raw) {
            if dry {
                let r = if m.replaces.is_empty() { "?".into() } else { m.replaces.join(",") };
                println!("  would merge {r} -> {}", m.text.chars().take(90).collect::<String>());
                queued += 1;
                continue;
            }
            let target = m.replaces.join(",");
            if let Some(line) = queue(send, "tidy", &ds, Some(&m.text), None,
                                      Some(target.as_str()).filter(|t| !t.is_empty()),
                                      Some(&m.rationale), "tidy") {
                println!("  {line}");
                queued += 1;
            }
        }
    }
    if unread > 0 {
        // "The model never answered" is not "the store is clean". Saying the first when
        // you mean the second is how a broken pass looks like a healthy one.
        println!("  ⚠ {unread} group(s) unread — the model did not answer");
    } else if queued == 0 {
        println!("  nothing to merge");
    }
    0
}

fn cmd_split<F>(args: &[String], db: &std::path::Path, send: &F) -> i32
where
    F: Fn(&Request) -> Option<Response>,
{
    let dry = flag(args, "--dry-run");
    let ds = value(args, "--dataset").unwrap_or_else(|| scope_dataset(db, scope_of(args).as_deref()));
    let min_chars = num(args, "--min-chars", 1200);
    let rows = facts(db, &ds, num(args, "--limit", 10), Some(min_chars));
    println!("{ds}: {} over-long fact(s) examined{}", rows.len(),
             if dry { " [dry-run]" } else { "" });
    let b = backend(db);
    let (mut queued, mut unread, mut refused, mut declined) = (0usize, 0usize, 0usize, 0usize);
    for f in &rows {
        let Some(raw) = lib::draft::complete(lib::prompts::SPLIT_SYS, &f.text, &b) else {
            unread += 1;
            continue;
        };
        match lib::upkeep::plan_split(&raw, &f.text) {
            Err(lib::upkeep::SplitRefusal::NotASplit) => declined += 1,
            Err(lib::upkeep::SplitRefusal::LostTooMuch) => refused += 1,
            Ok(parts) => {
                if dry {
                    println!("  {} -> {} parts", f.id.chars().take(8).collect::<String>(),
                             parts.len());
                    for p in &parts {
                        println!("     · {}", p.chars().take(110).collect::<String>());
                    }
                    queued += 1;
                    continue;
                }
                // ONE row for every part, joined by SPLIT_SEP. N rows pointing at one
                // original would delete it on the first approval and strand the rest.
                let text = parts.join(lib::SPLIT_SEP);
                let rationale = lib::upkeep::split_rationale(&f.text, parts.len());
                if let Some(line) = queue(send, "split", &ds, Some(&text), None, Some(&f.id),
                                          Some(&rationale), "split") {
                    println!("  {line}");
                    queued += 1;
                }
            }
        }
    }
    // Each counted APART. If the length guard is over-refusing, every over-long fact
    // stays bundled forever and the pass still looks like it is working.
    if unread > 0 {
        println!("  ⚠ {unread} unread — the model did not answer");
    }
    if refused > 0 {
        println!("  {refused} refused: the split lost >40% of the text (summarised, not split)");
    }
    if declined > 0 {
        println!("  {declined} judged coherent by the model — left alone");
    }
    if queued == 0 && unread == 0 {
        println!("  nothing worth splitting");
    }
    0
}

fn cmd_draft<F>(positional: &[String], args: &[String], db: &std::path::Path, send: &F) -> i32
where
    F: Fn(&Request) -> Option<Response>,
{
    let Some(notes) = positional.get(1).filter(|n| !n.trim().is_empty()) else {
        eprintln!("draft: needs <notes>");
        return 2;
    };
    let scope = scope_of(args);
    let b = backend(db);
    // fallback=true: hand-written notes. If the distiller is down, keep the operator's
    // own words rather than losing what they typed.
    let cands = lib::draft::distill(notes, scope.as_deref(), true, &b);
    queue_candidates(&cands, None, scope.as_deref(), db, send, "draft")
}

/// Shared by draft and dream: plan each candidate, then queue it.
fn queue_candidates<F>(cands: &[lib::draft::Candidate], session_dataset: Option<&str>,
                       scope: Option<&str>, db: &std::path::Path, send: &F, source: &str) -> i32
where
    F: Fn(&Request) -> Option<Response>,
{
    let origin = crate::git_origin().and_then(|o| paos_memory::scope::parse_origin(&o));
    let project = origin.as_ref().map(paos_memory::scope::project_dataset);
    let org = origin.as_ref().map(paos_memory::scope::org_dataset);
    let mut n = 0;
    for c in cands {
        let planned = lib::draft::plan(c, session_dataset, scope, project.as_deref(),
                                       org.as_deref(), &configured_global(db),
                                       near_duplicate(db, c, session_dataset, scope,
                                                      project.as_deref(), org.as_deref())
                                           .as_deref());
        if let Some(line) = queue(send, planned.kind, &planned.dataset, Some(&planned.text),
                                  Some(&planned.scope), planned.target_data_id.as_deref(),
                                  planned.rationale.as_deref(), source) {
            println!("  {line}");
            n += 1;
        }
    }
    if n == 0 {
        println!("nothing durable found");
    }
    0
}

/// The id of the nearest stored duplicate, if any is over the threshold.
///
/// Uses the difflib port, so the branch taken here is the same one the Python takes —
/// this decides `supersede` vs `capture`, which is what the human is asked to approve.
fn near_duplicate(db: &std::path::Path, c: &lib::draft::Candidate,
                  session_dataset: Option<&str>, scope: Option<&str>,
                  project: Option<&str>, org: Option<&str>) -> Option<String> {
    let (dataset, _) = lib::draft::target_dataset(
        session_dataset, c.scope.as_deref().or(scope), project, org,
        paos_memory::scope::DEFAULT_GLOBAL);
    let conn = read_only(db)?;
    let rows: Vec<(String, String)> = conn
        .prepare("SELECT id, text FROM memories WHERE dataset=?1 AND superseded IS NULL")
        .and_then(|mut st| {
            st.query_map([&dataset], |r| Ok((r.get(0)?, r.get(1)?))).and_then(|it| it.collect())
        })
        .unwrap_or_default();
    let threshold: f64 = std::env::var("COG_SUPERSEDE_THRESHOLD")
        .ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0.82);
    let mut best: Option<(f64, String)> = None;
    for (id, text) in rows {
        if text.is_empty() {
            continue;
        }
        let r = paos_memory::difflib::ratio(&c.text, &text);
        if r >= threshold && best.as_ref().is_none_or(|(b, _)| r > *b) {
            best = Some((r, id));
        }
    }
    best.map(|(_, id)| id)
}

fn cmd_lessons<F>(args: &[String], db: &std::path::Path, send: &F) -> i32
where
    F: Fn(&Request) -> Option<Response>,
{
    let dry = flag(args, "--dry-run");
    let limit = num(args, "--limit", 40);
    let min_sessions = num(args, "--min-sessions", lib::lessons::MIN_SESSIONS);
    let since = value(args, "--since");

    let listing = match paos_trajectory::list_trajectories(
        "claude-code", limit, None, since.as_deref(), None, now_secs()) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("lessons: {e}");
            return 1;
        }
    };
    // FIRST-ENCOUNTER order: the tie-break in `recurring` depends on it.
    let mut groups = lib::lessons::Groups::new();
    let mut episodes = 0usize;
    for item in &listing.items {
        let Ok(bytes) = std::fs::read(&item.path) else { continue };
        let raw = String::from_utf8_lossy(&bytes);
        let Ok(n) = paos_trajectory::normalize_transcript(
            "claude-code", &raw, paos_trajectory::DEFAULT_TRUNCATE) else { continue };
        if n.records.is_empty() {
            continue;
        }
        let (sid, cwd) = match &n.records[0] {
            paos_trajectory::Record::Meta(m) => (
                m.session_id.clone().unwrap_or_else(|| item.path.clone()), m.cwd.clone()),
            _ => (item.path.clone(), None),
        };
        let ds = lib::session::session_dataset(cwd.as_deref());
        for ep in paos_trajectory::failure_episodes(&n.records, 6) {
            episodes += 1;
            let g = groups.entry(&ep.signature);
            g.episodes.push(lib::lessons::Episode {
                tool: ep.tool, args: ep.args, error: ep.error,
                signature: ep.signature, recovery: ep.recovery,
            });
            g.sessions.insert(sid.clone());
            g.datasets.push(ds.clone());
        }
    }
    let ordered = groups.ordered();
    let uncapped = ordered.iter()
        .filter(|(s, g)| g.sessions.len() >= min_sessions && paos_trajectory::is_teachable(s))
        .count();
    let recur = lib::lessons::recurring(&ordered, min_sessions, paos_trajectory::is_teachable);

    // ALWAYS show the funnel. The whole design rests on recurrence filtering hard, and a
    // bare proposal count would hide it going slack — or shutting everything out.
    println!("{episodes} failure episode(s) → {} distinct signature(s) → {uncapped} recurring",
             groups.len());
    if dry {
        for (sig, g) in &recur {
            println!("  [{} sessions] {}", g.sessions.len(),
                     sig.chars().take(100).collect::<String>());
        }
        println!("re-run without --dry-run to write lessons for these");
        return 0;
    }
    if uncapped > lib::lessons::MAX_LESSONS {
        println!("note: capped at {} most-recurring (LESSON_MAX)", lib::lessons::MAX_LESSONS);
    }
    let b = backend(db);
    let mut n = 0;
    for (sig, g) in &recur {
        let Some(raw) = lib::draft::complete(lib::prompts::LESSON_SYS,
                                             &lib::lessons::evidence(g), &b) else {
            eprintln!("[librarian] lesson: LLM unavailable for {}",
                      sig.chars().take(60).collect::<String>());
            continue;
        };
        let cands = lib::draft::parse_candidates(&raw);
        if cands.is_empty() {
            // The prompt tells the model to return [] when the recovery shows no real
            // fix, so an empty reply is a legitimate VERDICT, not a malfunction. Say
            // which — failures here being silent is how "the librarian never worked"
            // went unnoticed for weeks.
            eprintln!("[librarian] lesson: no lesson for {} (model declined or unparseable)",
                      sig.chars().take(60).collect::<String>());
            continue;
        }
        let ds = lib::lessons::scope_dataset(&g.datasets);
        for c in &cands {
            let dataset = ds.clone().unwrap_or_else(|| configured_global(db));
            let scope = if ds.is_some() { "project" } else { "global" };
            let rationale = lib::lessons::rationale(c.rationale.as_deref(), g.sessions.len());
            if queue(send, "lesson", &dataset, Some(&c.text), Some(scope), None,
                     Some(&rationale), "lesson").is_some() {
                n += 1;
            }
        }
    }
    if n > 0 {
        println!("queued {n} lesson(s) — review with `paos memory review`");
    } else {
        println!("no lessons queued (nothing recurring, or the model declined)");
    }
    0
}

fn cmd_dream<F>(args: &[String], db: &std::path::Path, send: &F) -> i32
where
    F: Fn(&Request) -> Option<Response>,
{
    let dry = flag(args, "--dry-run");
    let limit = num(args, "--limit", 3);
    let since = value(args, "--since");
    let scope = scope_of(args);

    let paths: Vec<String> = match value(args, "--session") {
        Some(r) => match paos_trajectory::resolve_session(&r, None) {
            Ok(p) => vec![p],
            Err(e) => {
                eprintln!("dream: {e}");
                return 1;
            }
        },
        None => match paos_trajectory::list_trajectories(
            "claude-code", limit, None, since.as_deref(), None, now_secs()) {
            Ok(l) => l.items.into_iter().map(|i| i.path).collect(),
            Err(e) => {
                eprintln!("dream: {e}");
                return 1;
            }
        },
    };

    let b = backend(db);
    let chunk_size = lib::dream::chunk_chars(&b);
    let mut housekept = lib::dream::Housekept::new();
    let (mut total_chunks, mut silent, mut queued) = (0usize, 0usize, 0usize);

    if dry {
        println!("dry-run: would read {} session(s)", paths.len());
    }
    for path in &paths {
        let Ok(bytes) = std::fs::read(path) else {
            println!("  {path}: error reading");
            continue;
        };
        let raw = String::from_utf8_lossy(&bytes);
        let Ok(n) = paos_trajectory::normalize_transcript(
            "claude-code", &raw, lib::dream::TOOL_TRUNCATE) else { continue };
        if n.records.is_empty() {
            println!("  {path}: empty (skipped)");
            continue;
        }
        let (sid, cwd) = match &n.records[0] {
            paos_trajectory::Record::Meta(m) => (
                m.session_id.clone().unwrap_or_else(|| path.clone()), m.cwd.clone()),
            _ => (path.clone(), None),
        };
        let text = paos_trajectory::render_text(&n.records, lib::dream::TOOL_TRUNCATE);
        // Route this session's captures to ITS project brain, from the meta cwd — not the
        // daemon's cwd, and not the global brain.
        let sess_ds = lib::session::session_dataset(cwd.as_deref());
        let chunks = lib::dream::chunk_lines(&text, chunk_size);
        let used = chunks.len().min(lib::dream::MAX_CHUNKS);

        if dry {
            let cap = if chunks.len() > lib::dream::MAX_CHUNKS {
                format!("  (capped from {})", chunks.len())
            } else {
                String::new()
            };
            println!("  {sid}: {used} chunk(s), {} chars{cap}", text.chars().count());
            continue;
        }
        if chunks.len() > lib::dream::MAX_CHUNKS {
            println!("note: {sid} covered {used}/{} chunks (raise DREAM_MAX_CHUNKS for more)",
                     chunks.len());
        }
        for ch in chunks.iter().take(used) {
            total_chunks += 1;
            // fallback=FALSE: a raw transcript chunk must never be enqueued as a memory.
            let cands = lib::draft::distill(ch, scope.as_deref(), false, &b);
            if cands.is_empty() {
                silent += 1;
                continue;
            }
            queue_candidates(&cands, sess_ds.as_deref(), scope.as_deref(), db, send, "dream");
            queued += cands.len();
        }
        // Housekeep the scope we just wrote to, ONCE per dataset per run. Several
        // sessions commonly share a repo, and re-running these per session queued the
        // same merge two or three times.
        if let Some(ds) = sess_ds.as_deref() {
            if housekept.claim(ds) {
                let mut a = vec!["tidy".to_string(), "--dataset".into(), ds.into(),
                                 "--limit".into(), "40".into()];
                cmd_tidy(&a, db, send);
                a = vec!["split".into(), "--dataset".into(), ds.into(),
                         "--min-chars".into(), "1200".into(), "--limit".into(), "3".into()];
                cmd_split(&a, db, send);
            }
        }
    }
    if dry {
        println!("re-run without --dry-run to draft candidate memories");
        return 0;
    }
    if queued > 0 {
        println!("dreamed {} session(s) → queued {queued} proposal(s)", paths.len());
        println!("review: paos memory review   ·   approve: paos memory approve --all");
    } else {
        println!("dreamed {} session(s), {total_chunks} chunk(s) → no candidate memories",
                 paths.len());
        if total_chunks > 0 && silent == total_chunks {
            // The old message blamed the LOCAL model unconditionally, which is wrong
            // whenever the backend is `claude` — it sent you to check a model that was
            // not being used.
            println!("  every chunk came back empty. backend={b} — check that one, not the other.");
        } else {
            println!("  the sessions had nothing durable worth keeping.");
        }
    }
    0
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_flags_map_to_tiers() {
        assert_eq!(scope_of(&["--global".to_string()]).as_deref(), Some("global"));
        assert_eq!(scope_of(&["--org".to_string()]).as_deref(), Some("org"));
        assert_eq!(scope_of(&["--project".to_string()]).as_deref(), Some("project"));
        assert_eq!(scope_of(&[]), None);
    }

    #[test]
    fn numeric_flags_fall_back_rather_than_failing() {
        // A bad --limit must not abandon the pass; the default is a working value.
        assert_eq!(num(&["--limit".into(), "7".into()], "--limit", 60), 7);
        assert_eq!(num(&["--limit".into(), "oops".into()], "--limit", 60), 60);
        assert_eq!(num(&[], "--limit", 60), 60);
    }

    #[test]
    fn scope_dataset_never_returns_a_tuple_shaped_surprise() {
        // Python passed a (name, scope) TUPLE through here once, which made tidy find
        // zero facts and report "nothing to merge" — a clean bill of health that was
        // really a type error. A String cannot do that, and this test says why.
        // A path that does not exist: configured_global falls back rather than failing,
        // which is the behaviour a machine with an unreadable store needs anyway.
        let ds = scope_dataset(std::path::Path::new("/definitely/not/here"), Some("global"));
        assert_eq!(ds, paos_memory::scope::DEFAULT_GLOBAL);
        assert!(!ds.contains('('), "must be a dataset NAME, not a debug-formatted pair");
    }
}
