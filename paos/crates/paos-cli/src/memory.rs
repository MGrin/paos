//! `paos memory <verb>` — the nested surface every session and doc already types.
//!
//! Nested rather than flat because of the endgame: when the last facet lands, the Python
//! dispatcher goes away and this binary is installed AS `~/.claude/skills/paos/paos`. So
//! `paos memory forget <id> --force` has to resolve natively — the dashboard shells
//! exactly that string (paos-web `run_skill`), and it has no test that would notice a
//! rename.
//!
//! The flat `paos remember` / `recall` / `forget` verbs stay as aliases.
//!
//! READS open the database read-only and work in a sandbox. WRITES go through the daemon,
//! or the spool when the socket is blocked — never direct SQLite, because the daemon owns
//! the embedding and a fact embedded elsewhere lands in a different vector space and is
//! silently unfindable.

use paos_proto::{Request, Response};

const USAGE: &str = "\
usage: paos memory <command>

  remember <text> --global|--org|--project [--supersede <id[,id...]>] [--no-split-hint]
  recall <query> [--top-k N] [--dataset NAME]
  forget <id> [--force]        destructive: previews without --force
  list                         datasets and their sizes
  show <dataset>               every live fact in one dataset
  review [--all]               the human-gated proposal queue
  approve <id>...|--all
  reject <id>...
  draft <notes> [--global|--org|--project]
  dream [--since S] [--limit N] [--session REF] [--dry-run]
  lessons [--since S] [--limit N] [--min-sessions N] [--dry-run]
  tidy [--limit N] [--dry-run]
  split [--min-chars N] [--limit N] [--dry-run]
  graph                        (retired with cognee)
";

/// Long-fact hint threshold, `COG_LONG_FACT_CHARS`.
const LONG_FACT_CHARS: usize = 600;

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn read_only(db: &std::path::Path) -> Option<rusqlite::Connection> {
    rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
}

/// Dispatch. `send` performs a request (daemon, else spool/read-only fallback).
pub fn run<F>(positional: &[String], args: &[String], db: &std::path::Path, send: F) -> i32
where
    F: Fn(&Request) -> Option<Response>,
{
    let Some(sub) = positional.get(1).map(String::as_str) else {
        print!("{USAGE}");
        return 0;
    };
    match sub {
        "remember" => cmd_remember(db, positional.get(2).map(String::as_str), args, &send),
        "recall" => cmd_recall(positional.get(2).map(String::as_str), args, &send),
        "list" => cmd_list(db),
        "show" => cmd_show(db, positional.get(2).map(String::as_str)),
        "graph" => cmd_graph(),
        "forget" => cmd_forget(db, positional.get(2).map(String::as_str), flag(args, "--force"),
                               &send),
        "review" => cmd_review(db, flag(args, "--all")),
        "approve" | "reject" => cmd_decide(db, sub, positional, flag(args, "--all"), &send),
        // The librarian passes. All the judgement is in paos-librarian; this only routes.
        "draft" | "dream" | "lessons" | "tidy" | "split" | "phrasings" => {
            crate::librarian::run(sub, &positional[1..], args, db, send)
        }
        other => {
            eprintln!("memory: unknown command '{other}'");
            print!("{USAGE}");
            2
        }
    }
}

/// Store a fact, optionally retiring the ones it replaces.
///
/// `--supersede` takes a LIST. The daemon verb has taken one since the column existed,
/// but the flag passed a single id — so the capability was there and unreachable from the
/// one place a human would use it. Passing "a,b,c" to the old flag did not error: it
/// spooled the comma-joined string as ONE id, stored the replacement, matched nothing,
/// and reported success, leaving all four originals live beside a fifth near-identical
/// copy.
fn cmd_remember<F>(db: &std::path::Path, text: Option<&str>, args: &[String], send: &F) -> i32
where
    F: Fn(&Request) -> Option<Response>,
{
    let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
        eprintln!("memory: remember needs <text>");
        return 2;
    };
    let Some(tier) = tier_of(args) else {
        // No default and no fallback: a wrongly-global fact surfaces in every repo
        // forever.
        eprintln!("memory: remember needs --global, --org or --project");
        return 2;
    };
    let dataset = value(args, "--dataset");
    let supersede = value(args, "--supersede").map(|raw| parse_supersede(&raw));
    if matches!(&supersede, Some(v) if v.is_empty()) {
        eprintln!("--supersede needs at least one id");
        return 2;
    }

    let req = match &supersede {
        Some(old_ids) => Request::Supersede {
            old_ids: old_ids.clone(),
            tier: tier.clone(),
            origin: crate::git_origin(),
            text: text.to_string(),
            dataset: dataset.clone(),
        },
        None => Request::Remember {
            tier: tier.clone(),
            origin: crate::git_origin(),
            text: text.to_string(),
            dataset: dataset.clone(),
        },
    };
    // Declared without a value: every arm of the match below either sets it or returns,
    // so initialising it here is dead and the compiler says so.
    let stored_id: Option<String>;
    match send(&req) {
        Some(Response::Ok { lines }) => {
            for l in &lines {
                println!("{l}");
            }
            // The daemon answers `stored in <dataset> (<id>)`. Keep that id so the
            // duplicate scan below can exclude the row it just created.
            stored_id = lines.iter().find_map(|l| stored_id_of(l));
        }
        Some(Response::Err { message, exit_code }) => {
            eprintln!("memory: {message}");
            return exit_code;
        }
        None => {
            eprintln!("memory: paosd unreachable and the spool failed — NOTHING WAS STORED");
            return 1;
        }
    }
    let ds = dataset.unwrap_or_else(|| derived_dataset(db, &tier));
    post_write_hints(db, &ds, text, supersede.is_some(), stored_id.as_deref());
    // The write-time push-back. AFTER the store, never instead of it: a blocked remember
    // loses the fact, and the moment of writing is the only moment the author still has
    // the context to split it.
    if !flag(args, "--no-split-hint") && supersede.is_none() {
        // A SANDBOXED session spools, so the reply carries no id — and every agent session
        // on this machine is sandboxed. Falling back to "no id, no offer" would have made
        // this feature fire only from a terminal, i.e. never for the callers it is for.
        // Found by running it in a sandbox rather than reasoning about it.
        let id = stored_id.clone()
            .unwrap_or_else(|| paos_memory::stable_id(&ds, text));
        propose_split_inline(db, send, &id, text, &ds);
    }
    0
}

fn tier_of(args: &[String]) -> Option<String> {
    for (flag, tier) in [("--global", "global"), ("--org", "org"), ("--project", "project")] {
        if args.iter().any(|a| a == flag) {
            return Some(tier.into());
        }
    }
    None
}

/// The dataset a tier resolves to for THIS cwd — used only to aim the near-duplicate
/// scan, never to decide where the fact was stored (the daemon did that).
fn derived_dataset(db: &std::path::Path, tier: &str) -> String {
    let origin = crate::git_origin().and_then(|o| paos_memory::scope::parse_origin(&o));
    match (tier, origin.as_ref()) {
        ("project", Some(o)) => paos_memory::scope::project_dataset(o),
        ("org", Some(o)) => paos_memory::scope::org_dataset(o),
        // The CONFIGURED global. Aiming the near-duplicate scan at a dataset this
        // machine does not use finds no duplicates and reports none — a clean answer
        // that means nothing.
        _ => rusqlite::Connection::open_with_flags(
                db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()
            .map(|c| paos_memory::scope::global_dataset(&c))
            .unwrap_or_else(|| paos_memory::scope::DEFAULT_GLOBAL.to_string()),
    }
}

fn cmd_recall<F>(query: Option<&str>, args: &[String], send: &F) -> i32
where
    F: Fn(&Request) -> Option<Response>,
{
    let Some(query) = query else {
        eprintln!("memory: recall needs <query>");
        return 2;
    };
    let top_k = value(args, "--top-k").and_then(|v| v.parse().ok()).unwrap_or(8);
    let dataset = value(args, "--dataset");
    let all_scopes = flag(args, "--all-scopes");
    let origin = crate::git_origin();
    // SAY WHICH BRAINS WERE SEARCHED when it is not the ones you would assume.
    //
    // Outside a git repo there is no project or org to derive, so recall reads the global
    // brain alone — 186 facts of 1,260 on this machine — and said NOTHING. From a plain
    // terminal it therefore looked like a search of everything that simply found little,
    // which is indistinguishable from a memory that has forgotten what you asked about.
    if origin.is_none() && dataset.is_none() && !all_scopes {
        eprintln!("(searched the global brain only — not in a git repo; \
                   --all-scopes searches every brain)");
    }
    match send(&Request::Recall { origin, query: query.into(), top_k,
                                  dataset, all_scopes }) {
        Some(Response::Ok { lines }) => {
            for l in &lines {
                println!("{l}");
            }
            0
        }
        Some(Response::Err { message, exit_code }) => {
            eprintln!("memory: {message}");
            exit_code
        }
        None => {
            eprintln!("memory: paosd unreachable and paos.db unreadable");
            1
        }
    }
}

/// Datasets and their sizes.
fn cmd_list(db: &std::path::Path) -> i32 {
    let Some(c) = read_only(db) else {
        eprintln!("memory: paos.db unreadable");
        return 1;
    };
    let Ok(mut st) = c.prepare(
        "SELECT dataset, COUNT(*) n FROM memories WHERE superseded IS NULL \
         GROUP BY dataset ORDER BY n DESC",
    ) else {
        eprintln!("memory: paos.db unreadable");
        return 1;
    };
    let rows: Vec<(String, i64)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .and_then(|it| it.collect())
        .unwrap_or_default();
    if rows.is_empty() {
        println!("no memories yet");
        return 0;
    }
    let total: i64 = rows.iter().map(|r| r.1).sum();
    println!("{} dataset(s), {} fact(s):", rows.len(), total);
    for (ds, n) in &rows {
        println!("  {ds:<42} {n}");
    }
    0
}

/// Every live fact in one dataset.
fn cmd_show(db: &std::path::Path, name: Option<&str>) -> i32 {
    let Some(name) = name else {
        eprintln!("memory: show needs <dataset>");
        return 2;
    };
    let Some(c) = read_only(db) else {
        eprintln!("memory: paos.db unreadable");
        return 1;
    };
    let rows: Vec<(String, String)> = c
        .prepare(
            "SELECT id, text FROM memories WHERE dataset=?1 AND superseded IS NULL \
             ORDER BY created_ts DESC",
        )
        .and_then(|mut st| {
            st.query_map([name], |r| Ok((r.get(0)?, r.get(1)?)))
                .and_then(|it| it.collect())
        })
        .unwrap_or_default();
    if rows.is_empty() {
        // Name the datasets that DO exist. An empty result and a typo look identical
        // otherwise, and the dataset names are derived, not chosen, so they are easy to
        // get slightly wrong.
        let known: Vec<String> = c
            .prepare("SELECT DISTINCT dataset FROM memories WHERE superseded IS NULL ORDER BY 1")
            .and_then(|mut st| st.query_map([], |r| r.get(0)).and_then(|it| it.collect()))
            .unwrap_or_default();
        eprintln!("paos memory: no dataset '{name}' (known: {})", known.join(", "));
        return 1;
    }
    println!("{} item(s) in '{name}':", rows.len());
    for (id, text) in &rows {
        // The TEXT is the item. cognee stored an opaque filename and made you fetch the
        // content separately, which is why `show` used to tell you almost nothing.
        println!("  - [{id}] {}", text.chars().take(160).collect::<String>());
    }
    0
}

/// cognee's knowledge graph went with cognee.
fn cmd_graph() -> i32 {
    println!("`graph` described cognee's extracted entity graph. cognee has been retired;");
    println!("memory is now a local scoped vector store with no graph layer.");
    println!("Use: paos memory recall \"<query>\"  ·  paos memory list  ·  paos doctor");
    0
}

/// Delete one fact. THE PREVIEW IS MANDATORY.
///
/// SKILL.md promises every session that `forget` is gated — run it without `--force`
/// first and show the operator the preview. That gate lived only in the Python; the Rust
/// CLI hard-deleted on the spot. This is the only destructive verb an agent can reach, so
/// it is the one place where "the docs say it is safe" has to actually be true.
fn cmd_forget<F>(db: &std::path::Path, id: Option<&str>, force: bool, send: &F) -> i32
where
    F: Fn(&Request) -> Option<Response>,
{
    let Some(id) = id else {
        eprintln!("memory: forget needs <id>");
        return 2;
    };
    let row: Option<(String, String)> = read_only(db).and_then(|c| {
        c.query_row("SELECT dataset, text FROM memories WHERE id=?1", [id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .ok()
    });
    let Some((dataset, text)) = row else {
        // An unknown id is an ERROR, not a silent success: "forgot" for something that
        // was never there tells the operator a fact is gone when it may still be live
        // under a different id.
        eprintln!("paos memory: no memory with id '{id}'");
        return 1;
    };
    if !force {
        println!("would delete from '{dataset}':\n  {}",
                 text.chars().take(400).collect::<String>());
        println!("re-run with --force to delete.");
        // Non-zero so a script cannot mistake the preview for a completed delete.
        return 2;
    }
    match send(&Request::Forget { id: id.to_string() }) {
        Some(Response::Ok { lines }) => {
            for l in &lines {
                println!("{l}");
            }
            0
        }
        Some(Response::Err { message, exit_code }) => {
            eprintln!("memory: {message}");
            exit_code
        }
        None => {
            eprintln!("memory: paosd unavailable and the spool is unwritable — nothing deleted");
            1
        }
    }
}

/// The proposal queue, clean first and screen-flagged last.
///
/// Flagged proposals sort LAST rather than being hidden: screening is advisory, so the
/// queue becomes a triage instead of a linear read that gets abandoned partway.
fn cmd_review(db: &std::path::Path, all: bool) -> i32 {
    let Some(c) = read_only(db) else {
        eprintln!("memory: paos.db unreadable");
        return 1;
    };
    let rows = if all {
        paos_librarian::queue::list_all(&c, 200)
    } else {
        paos_librarian::queue::list_pending(&c)
    };
    let Ok(rows) = rows else {
        eprintln!("memory: paos.db unreadable");
        return 1;
    };
    if rows.is_empty() {
        println!("no proposals");
        return 0;
    }
    // Flagged ones LAST. The queue is read top-down and abandoned partway, so the order
    // decides what actually gets looked at — the likely-good facts should not sit
    // underneath a run of obvious status noise.
    let (clean, flagged): (Vec<_>, Vec<_>) = rows.iter().partition(|p| p.screen.is_none());
    for p in clean.iter().chain(flagged.iter()) {
        // A supersede proposal can carry no text of its own; name what it replaces
        // instead of printing a blank line.
        let owned;
        let body: &str = match p.text.as_deref().filter(|t| !t.is_empty()) {
            Some(t) => t,
            None => {
                owned = format!("supersede {}", p.target_data_id.as_deref().unwrap_or(""));
                &owned
            }
        };
        let note = if p.status == "pending" {
            String::new()
        } else {
            format!("  <{}>", p.status)
        };
        println!("#{} [{}] ({}) {}{note}", p.id, p.kind, p.dataset,
                 body.chars().take(100).collect::<String>());
        if let Some(why) = &p.screen_why {
            // Say WHY, and say it is a guess. A bare "likely noise" is a verdict the
            // reader cannot check, and this screen is advisory precisely because the
            // historical labels showed it cannot be trusted to decide on its own.
            println!("     ⚠ likely noise (advisory, you decide): {why}");
        }
    }
    if !flagged.is_empty() {
        println!("\n{} of {} flagged as likely noise and sorted last.",
                 flagged.len(), rows.len());
    }
    0
}

/// approve / reject.
///
/// `paos memory approve <id>` and `memory reject <id>` are shelled VERBATIM by the
/// dashboard (paos-web `run_skill`), which has no test that would notice a rename.
fn cmd_decide<F>(
    db: &std::path::Path,
    action: &str,
    positional: &[String],
    all: bool,
    send: &F,
) -> i32
where
    F: Fn(&Request) -> Option<Response>,
{
    let Some(c) = read_only(db) else {
        eprintln!("memory: paos.db unreadable");
        return 1;
    };
    let ids: Vec<i64> = if all {
        match paos_librarian::queue::list_pending(&c) {
            Ok(rows) => rows.iter().map(|p| p.id).collect(),
            Err(_) => {
                eprintln!("memory: paos.db unreadable");
                return 1;
            }
        }
    } else {
        // Non-numeric arguments are SKIPPED rather than fatal, so one typo in a list does
        // not abandon the rest.
        positional[2..].iter().filter_map(|s| s.parse().ok()).collect()
    };
    if ids.is_empty() {
        eprintln!("memory: {action} needs <id>... or --all");
        return 2;
    }

    let mut worst = 0;
    for id in ids {
        if action == "reject" {
            worst = worst.max(decide(send, id, "rejected"));
            continue;
        }
        let Ok(Some(p)) = paos_librarian::queue::get(&c, id) else {
            eprintln!("#{id}: no such proposal");
            worst = worst.max(1);
            continue;
        };
        let alive = |fid: &str| paos_librarian::queue::fact_exists(&c, fid);
        match paos_librarian::apply::plan_apply(&p, alive) {
            Err(e) => {
                if paos_librarian::apply::refusal_retires(&e) {
                    // Every source is gone: retire the proposal instead of applying it,
                    // or approving a split of a deleted entry puts its pieces back.
                    println!("#{id}: retired — every fact it would replace is already gone");
                    worst = worst.max(decide(send, id, "rejected"));
                } else {
                    eprintln!("#{id}: cannot apply: {e:?}");
                    worst = worst.max(1);
                }
            }
            Ok(steps) => {
                let mut ok = true;
                for step in &steps {
                    let req = match step {
                        paos_librarian::apply::Step::Store { dataset, text } => {
                            Request::Remember {
                                tier: "project".into(),
                                origin: None,
                                text: text.clone(),
                                dataset: Some(dataset.clone()),
                            }
                        }
                        paos_librarian::apply::Step::StoreAndRetire {
                            dataset, text, old_ids,
                        } => Request::Supersede {
                            old_ids: old_ids.clone(),
                            tier: "project".into(),
                            origin: None,
                            text: text.clone(),
                            dataset: Some(dataset.clone()),
                        },
                    };
                    match send(&req) {
                        Some(Response::Ok { .. }) => {}
                        _ => {
                            // STAYS PENDING. A partially applied split must be
                            // retryable, and the ordering guarantees the original is
                            // still intact at this point.
                            eprintln!("#{id}: daemon unavailable mid-apply — original kept, \
                                       stays pending");
                            ok = false;
                            worst = worst.max(1);
                            break;
                        }
                    }
                }
                if ok {
                    worst = worst.max(decide(send, id, "approved"));
                }
            }
        }
    }
    worst
}

fn decide<F>(send: &F, id: i64, status: &str) -> i32
where
    F: Fn(&Request) -> Option<Response>,
{
    match send(&Request::ProposalSetStatus { id, status: status.into() }) {
        Some(Response::Ok { lines }) => {
            for l in &lines {
                println!("{l}");
            }
            0
        }
        Some(Response::Err { message, exit_code }) => {
            eprintln!("#{id}: {message}");
            exit_code
        }
        None => {
            eprintln!("#{id}: paosd unavailable and the spool is unwritable");
            1
        }
    }
}


/// Pull the fact id out of the daemon's `stored in <dataset> (<id>)` reply.
///
/// Parsed rather than plumbed through the protocol because the reply is deliberately
/// pre-rendered lines — see paos-proto. Returns None on anything unexpected, which
/// degrades to the previous behaviour rather than excluding the wrong row.
fn stored_id_of(line: &str) -> Option<String> {
    if !line.starts_with("stored in ") {
        return None;
    }
    let open = line.find('(')?;
    let close = line[open + 1..].find(')')? + open + 1;
    let id = &line[open + 1..close];
    // Ids are hex; anything else means the format moved and we should not guess.
    if !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(id.to_string())
    } else {
        None
    }
}

/// The near-duplicate warning and the long-fact hint, after a successful store.
///
/// Checked AFTER the write so a slow scan never delays storing, and skipped on an
/// explicit supersede — the caller already said which fact it replaces.
///
/// `just_stored` is the id the daemon returned, and it MUST be excluded: the row is
/// already in the table by the time this runs, so without it every single `remember`
/// reported the fact as a 100% near-duplicate OF ITSELF and told you to forget the id it
/// had just created. Following that advice deletes the fact you just wrote — and the
/// warning fires on every write, which trains everyone to ignore it, so a REAL duplicate
/// goes unread too. That second effect is the worse one.
///
/// `None` on the spool path: the row is not in the table yet, so there is nothing to
/// exclude and nothing to match.
pub fn post_write_hints(db: &std::path::Path, dataset: &str, text: &str, superseded: bool,
                        just_stored: Option<&str>) {
    // The bare hint stays ONLY for the supersede path, where no split is offered. On the
    // ordinary path `propose_split_inline` speaks instead, and printing both would restate
    // the problem immediately before offering the solution.
    if superseded && text.chars().count() > LONG_FACT_CHARS {
        println!(
            "  hint: this fact is long ({} chars) — memory works best with SHORT, atomic \
             facts (one idea each). Consider splitting it into separate `remember`s.",
            text.chars().count()
        );
    }
    if superseded {
        return;
    }
    let Some(c) = read_only(db) else { return };
    let rows: Vec<(String, String)> = c
        .prepare("SELECT id, text FROM memories WHERE dataset=?1 AND superseded IS NULL")
        .and_then(|mut st| {
            st.query_map([dataset], |r| Ok((r.get(0)?, r.get(1)?))).and_then(|it| it.collect())
        })
        .unwrap_or_default();
    let mut hits: Vec<(f64, String, String)> = rows
        .into_iter()
        .filter(|(id, t)| !t.is_empty() && Some(id.as_str()) != just_stored)
        .map(|(id, t)| (paos_memory::difflib::ratio(text, &t), id, t))
        .filter(|(r, _, _)| *r >= supersede_threshold())
        .collect();
    hits.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (ratio, id, t) in hits.into_iter().take(3) {
        println!(
            "⚠ near-duplicate already stored ({:.0}%) — consider `paos memory forget {id}`:",
            ratio * 100.0
        );
        println!("    {}", t.replace('\n', " ").chars().take(80).collect::<String>());
    }
}

fn supersede_threshold() -> f64 {
    std::env::var("COG_SUPERSEDE_THRESHOLD")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0.82)
}

/// `--supersede a,b,c` → the ids, ignoring blanks.
///
/// A LIST, because a tidy merges several facts into one. Passing "a,b,c" to a flag that
/// took a single id did not error — it spooled the comma-joined string as ONE id, stored
/// the replacement, matched nothing, and reported success, leaving all the originals live
/// beside a near-identical copy.
pub fn parse_supersede(raw: &str) -> Vec<String> {
    raw.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supersede_takes_a_list_and_tolerates_spaces() {
        assert_eq!(parse_supersede("a,b,c"), vec!["a", "b", "c"]);
        assert_eq!(parse_supersede(" a , b "), vec!["a", "b"]);
        assert_eq!(parse_supersede("a"), vec!["a"]);
        assert!(parse_supersede("").is_empty());
        assert!(parse_supersede(" , ").is_empty(), "blanks must not become ids");
    }

    fn db_with(rows: &[(&str, &str, &str)]) -> (tempdir::Dir, std::path::PathBuf) {
        let d = tempdir::Dir::new("memcli");
        let path = d.path().join("paos.db");
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(
            "CREATE TABLE memories(id TEXT PRIMARY KEY, dataset TEXT, text TEXT,
               embedding BLOB, created_ts TEXT, superseded TEXT);",
        )
        .unwrap();
        for (id, ds, text) in rows {
            c.execute(
                "INSERT INTO memories VALUES(?1,?2,?3,x'',?4,NULL)",
                rusqlite::params![id, ds, text, "T"],
            )
            .unwrap();
        }
        (d, path)
    }

    /// A scratch directory that cleans itself up. No PAOS_ROOT, no env mutation.
    mod tempdir {
        pub struct Dir(std::path::PathBuf);
        impl Dir {
            pub fn new(tag: &str) -> Self {
                let base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
                let p = std::path::Path::new(&base).join(format!(
                    "paos-memcli-{tag}-{}-{:?}",
                    std::process::id(),
                    std::thread::current().id()
                ));
                let _ = std::fs::remove_dir_all(&p);
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn forget_without_force_previews_and_exits_nonzero() {
        let (_d, db) = db_with(&[("f1", "ds", "a fact worth keeping")]);
        let called = std::cell::Cell::new(false);
        let code = cmd_forget(&db, Some("f1"), false, &|_r: &Request| {
            called.set(true);
            Some(Response::ok("deleted"))
        });
        assert_eq!(code, 2, "a preview must not look like a completed delete");
        assert!(!called.get(), "and must not reach the daemon at all");
    }

    #[test]
    fn forget_with_force_reaches_the_daemon() {
        let (_d, db) = db_with(&[("f1", "ds", "a fact")]);
        let seen = std::cell::RefCell::new(None);
        let code = cmd_forget(&db, Some("f1"), true, &|r: &Request| {
            *seen.borrow_mut() = Some(r.clone());
            Some(Response::ok("forgot f1"))
        });
        assert_eq!(code, 0);
        assert_eq!(seen.into_inner(), Some(Request::Forget { id: "f1".into() }));
    }

    #[test]
    fn forget_of_an_unknown_id_is_an_error_not_a_silent_success() {
        let (_d, db) = db_with(&[("f1", "ds", "a fact")]);
        let code = cmd_forget(&db, Some("nope"), true, &|_: &Request| {
            panic!("must not reach the daemon for an id that does not exist")
        });
        assert_eq!(code, 1);
    }

    #[test]
    fn list_and_show_ignore_retired_facts() {
        let (_d, db) = db_with(&[("f1", "ds", "live"), ("f2", "ds", "retired")]);
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute("UPDATE memories SET superseded='f1' WHERE id='f2'", []).unwrap();
        drop(c);
        // Both go through the same `superseded IS NULL` filter every reader uses.
        assert_eq!(cmd_list(&db), 0);
        assert_eq!(cmd_show(&db, Some("ds")), 0);
        assert_eq!(cmd_show(&db, Some("nope")), 1, "an unknown dataset is an error");
    }

    #[test]
    fn graph_is_a_tombstone_that_points_somewhere_useful() {
        assert_eq!(cmd_graph(), 0);
    }

    #[test]
    fn the_stored_id_is_parsed_out_of_the_daemon_reply() {
        assert_eq!(stored_id_of("stored in global_memory (0c61ae8063112c47)").as_deref(),
                   Some("0c61ae8063112c47"));
        // The supersede reply carries more after the id and must still parse.
        assert_eq!(stored_id_of("stored in ds (abc123); retired 2 fact(s)").as_deref(),
                   Some("abc123"));
    }

    #[test]
    fn anything_unexpected_yields_none_rather_than_a_guess() {
        // Degrading to "exclude nothing" restores the old behaviour — noisy, but it
        // cannot exclude the WRONG row, which would hide a real duplicate.
        assert!(stored_id_of("spooled — paosd applies it within ~5s").is_none());
        assert!(stored_id_of("stored in ds (not-hex-zzz)").is_none());
        assert!(stored_id_of("stored in ds ()").is_none());
        assert!(stored_id_of("stored in ds").is_none());
    }

}


/// Offer a split at the moment of writing, when the author still has the context.
///
/// WHY THIS EXISTS, and why it is not a louder hint. The long-fact hint has fired on every
/// over-600 write for weeks and has never once been acted on — measured: 13 of 13 ignored
/// in one afternoon, including twice by me, consciously, with a reason I still think was
/// defensible in one of the two cases. So it is not unseen and it is not misread.
///
/// The reason is economics, not attention. Complying means re-reading what you just wrote,
/// deciding the seams, composing N `remember` calls and re-deriving the scope for each —
/// several turns. Declining costs nothing, because the fact is ALREADY STORED. A rational
/// author declines every time.
///
/// So this does not raise the price of storing; it lowers the price of splitting. The
/// expensive half is done for the author and accepting is one command. A refusal would
/// have done the opposite — and worse, it would push authors to pad facts down under the
/// limit, destroying information to satisfy a number.
///
/// Three properties it must keep:
///   * the store ALREADY SUCCEEDED before this runs. An unavailable model, a failed queue
///     write, anything at all here — none of it may cost the fact.
///   * the proposal is queued, so ignoring it is not the same as losing it: it turns up in
///     `paos memory review` like any other.
///   * it reuses SPLIT_SYS, plan_split and the apply path unchanged, so the inline offer
///     and the batch pass cannot drift apart.
fn propose_split_inline<F>(db: &std::path::Path, send: &F, stored_id: &str, text: &str,
                           dataset: &str)
where
    F: Fn(&Request) -> Option<Response>,
{
    if text.chars().count() <= LONG_FACT_CHARS {
        return;
    }
    let backend = {
        let configured = read_only(db).and_then(|c| paos_librarian::llm::configured_backend(&c));
        paos_librarian::llm::resolve_backend(configured.as_deref())
    };
    let Some(raw) = paos_librarian::draft::complete(
        paos_librarian::prompts::SPLIT_SYS, text, &backend) else {
        // Silent on purpose: the fact is stored, and a model outage is not the author's
        // problem to read about mid-write.
        return;
    };
    let parts = match paos_librarian::upkeep::plan_split(&raw, text) {
        Ok(p) => p,
        // The model judged it coherent, or its answer lost too much. Either way there is
        // nothing to offer, and saying so would be the wallpaper this replaces.
        Err(_) => return,
    };

    println!("  this fact is {} chars. A split into {} atomic facts is available:",
             text.chars().count(), parts.len());
    for (i, p) in parts.iter().enumerate() {
        println!("    {}. ({}) {}", i + 1, p.chars().count(),
                 p.chars().take(88).collect::<String>());
    }
    let joined = parts.join(paos_librarian::SPLIT_SEP);
    let rationale = paos_librarian::upkeep::split_rationale(text, parts.len());
    let req = Request::ProposalAdd {
        kind: "split".into(),
        dataset: dataset.into(),
        text: Some(joined),
        scope: None,
        target_data_id: Some(stored_id.into()),
        rationale: Some(rationale),
        source: Some("write-time".into()),
    };
    match send(&req) {
        // The daemon answers `proposal <id>`; a sandboxed session gets `spooled — ...`
        // and there is no id yet, so do not invent one.
        Some(Response::Ok { lines }) => {
            let id = lines.iter().find_map(|l| l.strip_prefix("proposal ")).map(str::trim);
            match id {
                Some(id) => println!("    apply: paos memory approve {id}   ·   keep as one: do nothing"),
                None => println!("    queued — it will appear in: paos memory review"),
            }
        }
        _ => {
            // Could not queue it. Say so, because otherwise the offer above is a lie.
            println!("    (could not queue the split — the fact is stored, nothing was lost)");
        }
    }
}
