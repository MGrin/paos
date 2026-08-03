//! `paos task` — the fleet's shared work queue.
//!
//! Reads open the database read-only and never touch the daemon socket, so `list`,
//! `ready` and `show` keep working inside an agent sandbox where that socket is blocked.
//! Writes go through the daemon, spooling when it is unreachable — with ONE exception,
//! `claim`, which cannot be fire-and-forget. See [`claim_and_confirm`].

use paos_proto::{Request, Response};
use paos_tasks::model::{Origin, State};
use paos_tasks::{query, store};

/// Derive (scope, org, repo) exactly the way memory does — including the failure.
///
/// `--project`/`--org` outside a git repo ERROR rather than silently becoming global.
/// Memory learned this the hard way, and for tasks the consequence is worse than a
/// misfiled fact: a wrongly-global task is work that nobody in the repo it belongs to
/// will ever see in `paos task ready`.
pub(crate) fn resolve_scope(
    flag: Option<&str>,
    origin: Option<&str>,
) -> Result<(String, Option<String>, Option<String>), String> {
    let parsed = origin.and_then(paos_memory::scope::parse_origin);
    match flag {
        Some("global") => Ok(("global".into(), None, None)),
        Some("org") => match parsed {
            Some(o) => Ok(("org".into(), Some(o.owner), None)),
            None => Err("no git 'origin' remote here — can't use --org; use --global".into()),
        },
        Some("project") => match parsed {
            Some(o) => Ok(("project".into(), Some(o.owner), Some(o.repo))),
            None => Err("no git 'origin' remote here — can't use --project; use --global".into()),
        },
        Some(other) => Err(format!("unknown scope: {other} (want global|org|project)")),
        // No flag: narrowest that is still true, same instinct as memory. Inside a repo
        // that is the repo; outside one there is nothing narrower than global.
        None => match parsed {
            Some(o) => Ok(("project".into(), Some(o.owner), Some(o.repo))),
            None => Ok(("global".into(), None, None)),
        },
    }
}

const USAGE: &str = "\
paos task — the fleet's shared work queue

  paos task create <title> [--body <text>|-] [--global|--org|--project]
                           [--parent <id>] [-p 0..3] [--dep <id>] [--room <r>] [--ready]
  paos task ready   [--all] [--json]        claimable work, rescues first
  paos task claim   <id>                    atomic; tells you if you lost
  paos task release <id>                    give it back, keeping its progress
  paos task show    <id>                    the briefing
  paos task list    [--state <s>] [--mine] [--orphaned] [--all] [--json]
  paos task note    <id> <text>
  paos task review  <id>                    hand it to the operator
  paos task close   <id>
  paos task drop    <id>
  paos task grant   <id>                    operator: let a session close this
  paos task dep     add|rm <id> <other-id>
";

/// argv, parsed once.
///
/// A struct rather than a pile of closures so the subcommands can be plain functions —
/// `&dyn Fn` parameters made the borrow checker's lifetimes the caller's problem for no
/// benefit.
pub(crate) struct Args<'a> {
    flags: Vec<&'a str>,
    values: Vec<(&'a str, &'a str)>,
    positional: Vec<&'a str>,
}

/// Flags that consume the next argument. Anything not here is a bare flag, and getting
/// this wrong silently turns a flag's value into a positional — which is how
/// `--ppid 4242` once came within a commit of minting a session named "4242".
const TAKES_VALUE: [&str; 7] =
    ["--body", "--parent", "--dep", "--room", "--state", "-p", "--priority"];

impl<'a> Args<'a> {
    pub(crate) fn parse(rest: &'a [String]) -> Args<'a> {
        let (mut flags, mut values, mut positional) = (Vec::new(), Vec::new(), Vec::new());
        let mut i = 0;
        while i < rest.len() {
            let a = rest[i].as_str();
            if TAKES_VALUE.contains(&a) {
                if let Some(v) = rest.get(i + 1) {
                    values.push((a, v.as_str()));
                }
                i += 2;
                continue;
            }
            if a.starts_with('-') && a != "-" {
                flags.push(a);
            } else {
                positional.push(a);
            }
            i += 1;
        }
        Args { flags, values, positional }
    }

    fn flag(&self, f: &str) -> bool {
        self.flags.contains(&f)
    }
    fn val(&self, f: &str) -> Option<&'a str> {
        self.values.iter().find(|(k, _)| *k == f).map(|(_, v)| *v)
    }
    fn at(&self, n: usize) -> &'a str {
        self.positional.get(n).copied().unwrap_or("")
    }
    fn rest_from(&self, n: usize) -> String {
        self.positional.get(n..).unwrap_or(&[]).join(" ")
    }
}

pub fn run(args: &[String]) -> Response {
    let sub = args.first().map(String::as_str).unwrap_or("");
    let rest = if args.is_empty() { &args[0..0] } else { &args[1..] };
    let a = Args::parse(rest);

    match sub {
        "" | "--help" | "-h" | "help" => Response::ok(USAGE),
        "create" => cmd_create(&a),
        "ready" => cmd_ready(a.flag("--all"), a.flag("--json")),
        "claim" => need_id(a.at(0), |id| claim_and_confirm(id, &me())),
        "release" => need_id(a.at(0), |id| {
            let who = me();
            if let Some(r) = precheck(|c| store::precheck_release(c, id, &who)) { return r; }
            write(Request::TaskRelease { id: id.into(), session: who })
        }),
        "show" => need_id(a.at(0), cmd_show),
        "list" => cmd_list(&a),
        "note" => {
            let (id, text) = (a.at(0), a.rest_from(1));
            if id.is_empty() || text.trim().is_empty() {
                return Response::err("usage: paos task note <id> <text>", 2);
            }
            write(Request::TaskNote { id: id.into(), author: me(), text })
        }
        "review" => need_id(a.at(0), |id| state_to(id, State::Review)),
        "close" => need_id(a.at(0), |id| state_to(id, State::Done)),
        "drop" => need_id(a.at(0), |id| state_to(id, State::Dropped)),
        "grant" => need_id(a.at(0), |id| write(Request::TaskGrant { id: id.into() })),
        "dep" => {
            let (verb, id, other) = (a.at(0), a.at(1), a.at(2));
            if !matches!(verb, "add" | "rm") || id.is_empty() || other.is_empty() {
                return Response::err("usage: paos task dep add|rm <id> <other-id>", 2);
            }
            if verb == "add" {
                if let Some(r) = precheck(|c| store::precheck_dep(c, id, other)) { return r; }
            }
            write(Request::TaskDep {
                id: id.into(),
                depends_on: other.into(),
                remove: verb == "rm",
            })
        }
        other => Response::err(format!("unknown: paos task {other}\n\n{USAGE}"), 2),
    }
}

fn need_id(id: &str, f: impl FnOnce(&str) -> Response) -> Response {
    if id.is_empty() {
        return Response::err("which task? pass an id — `paos task list` shows them", 2);
    }
    f(id)
}

/// This session's bus handle — what a claim is recorded against.
///
/// `PAOS_ACTOR=operator` is how the operator's own terminal identifies itself; without it
/// every task would be session-origin and the close-authority split in `may_close` would
/// never engage.
fn me() -> String {
    if std::env::var("PAOS_ACTOR").as_deref() == Ok("operator") {
        return "operator".into();
    }
    match crate::bus::whoami(None) {
        Some(Response::Ok { lines }) => lines
            .first()
            .map(|l| l.split_whitespace().next().unwrap_or(l).to_string())
            .unwrap_or_else(|| "unknown-session".into()),
        _ => "unknown-session".into(),
    }
}

fn ro() -> Result<rusqlite::Connection, Response> {
    paos_bus::readonly::open_ro(&paos_store::db_path()).ok_or_else(|| {
        Response::err(
            format!("paos.db is unreadable at {}", paos_store::db_path().display()),
            1,
        )
    })
}

/// Every write but `claim` — socket first, spool when it is blocked. Exactly the path
/// `bus send` and `memory remember` take.
fn write(req: Request) -> Response {
    crate::send_or_spool(&req)
        .unwrap_or_else(|| Response::err("daemon unreachable and the write could not be spooled", 69))
}

/// The spool payload for each task verb.
///
/// These op names must match the arms in `paosd::dream::apply_bus_op`. Nothing in the
/// compiler links the two, and an op with no arm there is quarantined as malformed — from
/// here that is indistinguishable from success, so the task simply never appears.
pub(crate) fn degraded_task(req: &Request) -> Option<serde_json::Value> {
    use serde_json::json;
    Some(match req {
        Request::TaskCreate { title, body, scope, org, repo, parent_id, priority, origin,
                              created_by, room, start_ready } => json!({
            "op": "task_create", "title": title, "body": body, "scope": scope, "org": org,
            "repo": repo, "parent_id": parent_id, "priority": priority, "origin": origin,
            "created_by": created_by, "room": room, "start_ready": start_ready,
        }),
        Request::TaskClaim { id, session } =>
            json!({ "op": "task_claim", "id": id, "session": session }),
        Request::TaskRelease { id, session } =>
            json!({ "op": "task_release", "id": id, "session": session }),
        Request::TaskState { id, to, actor } =>
            json!({ "op": "task_state", "id": id, "to": to, "actor": actor }),
        Request::TaskNote { id, author, text } =>
            json!({ "op": "task_note", "id": id, "author": author, "text": text }),
        Request::TaskGrant { id } => json!({ "op": "task_grant", "id": id }),
        Request::TaskDep { id, depends_on, remove } =>
            json!({ "op": "task_dep", "id": id, "depends_on": depends_on, "remove": remove }),
        _ => return None,
    })
}

/// Ask the policy before spooling.
///
/// A spooled write's only answer is "spooled". Without this, `paos task close` on a task
/// the session may not close returns exit 0 and the daemon silently refuses — the session
/// walks away believing the work is signed off. Caught by hand on 2026-08-02, after
/// `claim_and_confirm` had already been written for the identical hole.
///
/// The check runs over the read-only connection and calls the SAME `may_close` the daemon
/// will; the daemon is still the enforcer. This only means the caller is told now instead
/// of never.
fn precheck(f: impl FnOnce(&rusqlite::Connection) -> Result<(), String>) -> Option<Response> {
    let conn = ro().ok()?;
    match f(&conn) {
        Ok(()) => None,
        Err(e) => Some(Response::err(e, 1)),
    }
}

fn state_to(id: &str, to: State) -> Response {
    let actor = me();
    let a = if actor == "operator" {
        store::Actor::Operator
    } else {
        store::Actor::Session(&actor)
    };
    if let Some(refusal) = precheck(|c| store::precheck_state(c, id, to, &a)) {
        return refusal;
    }
    write(Request::TaskState {
        id: id.into(),
        to: to.as_str().into(),
        actor: actor.clone(),
    })
}

fn cmd_create(a: &Args) -> Response {
    let title = a.rest_from(0);
    if title.trim().is_empty() {
        return Response::err("usage: paos task create <title> [--body <text>]", 2);
    }
    let scope_flag = if a.flag("--global") { Some("global") }
                     else if a.flag("--org") { Some("org") }
                     else if a.flag("--project") { Some("project") }
                     else { None };
    let (scope, org, repo) = match resolve_scope(scope_flag, crate::git_origin().as_deref()) {
        Ok(t) => t,
        Err(e) => return Response::err(e, 2),
    };
    let body = match a.val("--body") {
        Some("-") => {
            let mut s = String::new();
            use std::io::Read;
            let _ = std::io::stdin().read_to_string(&mut s);
            Some(s).filter(|s| !s.trim().is_empty())
        }
        Some(b) => Some(b.to_string()),
        None => None,
    };
    let priority = a.val("-p")
        .or_else(|| a.val("--priority"))
        .and_then(|p| p.parse::<i64>().ok())
        .unwrap_or(2);
    // The operator creating from a terminal is the operator; a session is a session.
    // This is what decides who may close it later, so it is not a cosmetic label.
    let created_by = me();
    let origin = if created_by == "operator" { Origin::Operator } else { Origin::Session };

    let created = write(Request::TaskCreate {
        title: title.trim().into(),
        body,
        scope,
        org,
        repo,
        parent_id: a.val("--parent").map(str::to_string),
        priority,
        origin: origin.as_str().into(),
        created_by,
        room: a.val("--room").map(str::to_string),
        start_ready: a.flag("--ready"),
    });
    // A dependency named at creation only lands once the task exists. When the write
    // spooled we do not have an id yet, so say so rather than dropping it silently.
    if let Some(dep) = a.val("--dep") {
        if let Response::Ok { lines } = &created {
            match lines.first().filter(|l| l.starts_with("t-")) {
                Some(id) => {
                    let _ = write(Request::TaskDep {
                        id: id.clone(),
                        depends_on: dep.into(),
                        remove: false,
                    });
                }
                None => {
                    return Response::Ok {
                        lines: lines
                            .iter()
                            .cloned()
                            .chain(std::iter::once(format!(
                                "note: --dep {dep} was NOT applied — the create is still \
                                 queued, so there is no id yet. Add it with \
                                 `paos task dep add <id> {dep}` once it lands."
                            )))
                            .collect(),
                    }
                }
            }
        }
    }
    created
}

fn cmd_ready(all: bool, json: bool) -> Response {
    let conn = match ro() {
        Ok(c) => c,
        Err(e) => return e,
    };
    let repo = if all { None } else { current_repo() };
    match query::ready(&conn, repo.as_deref()) {
        Err(e) => Response::err(e, 1),
        Ok(ts) if ts.is_empty() => Response::ok(match (&repo, all) {
            (Some(r), _) => format!("no claimable work in {r} — try `paos task ready --all`"),
            _ => "no claimable work anywhere".into(),
        }),
        Ok(ts) => {
            if json {
                return Response::ok(json_tasks(&ts));
            }
            let mut lines = Vec::new();
            for t in &ts {
                // A rescue is the most valuable row here, so it is MARKED and not merely
                // sorted first — the sort is invisible the moment the list scrolls.
                let tag = if t.is_rescue() {
                    format!(" ⤺ rescue, last held by {}", t.last_owner.as_deref().unwrap_or("?"))
                } else {
                    String::new()
                };
                let where_ = t.repo.as_deref().map(|r| format!(" [{r}]")).unwrap_or_default();
                lines.push(format!("{}  p{}  {}{where_}{tag}", t.id, t.priority, t.title));
            }
            lines.push(String::new());
            lines.push("claim one with `paos task claim <id>`".into());
            Response::Ok { lines }
        }
    }
}

fn cmd_list(a: &Args) -> Response {
    let conn = match ro() {
        Ok(c) => c,
        Err(e) => return e,
    };
    let f = query::Filter {
        scope: None,
        repo: if a.flag("--all") { None } else { current_repo() },
        state: a.val("--state").and_then(State::parse),
        mine: if a.flag("--mine") { Some(me()) } else { None },
        orphaned_only: a.flag("--orphaned"),
    };
    match query::list(&conn, &f) {
        Err(e) => Response::err(e, 1),
        Ok(ts) if ts.is_empty() => Response::ok("nothing matches"),
        Ok(ts) => {
            if a.flag("--json") {
                return Response::ok(json_tasks(&ts));
            }
            let blocked = query::blocked_ids(&conn).unwrap_or_default();
            let lines = ts
                .iter()
                .map(|t| {
                    let owner = match (&t.claimed_by, &t.last_owner) {
                        (Some(w), _) => format!(" @{w}"),
                        (None, Some(p)) if t.orphaned => format!(" ⤺ was @{p}"),
                        _ => String::new(),
                    };
                    let b = if blocked.contains(&t.id) { " ⛔blocked" } else { "" };
                    format!("{}  {:<11} p{}  {}{owner}{b}",
                            t.id, t.state.as_str(), t.priority, t.title)
                })
                .collect();
            Response::Ok { lines }
        }
    }
}

/// The briefing.
///
/// Order is fixed and deliberate: state, then WHY it is unowned, then the body, then
/// dependencies, then the log. A rescuing session reads top to bottom and needs to know
/// what it is inheriting before it reads what the work is.
fn cmd_show(id: &str) -> Response {
    let conn = match ro() {
        Ok(c) => c,
        Err(e) => return e,
    };
    let t = match store::get(&conn, id) {
        Ok(Some(t)) => t,
        Ok(None) => return Response::err(format!("no such task: {id}"), 1),
        Err(e) => return Response::err(e, 1),
    };
    let mut l = vec![
        format!("{}  [{}]  p{}", t.id, t.state.as_str(), t.priority),
        t.title.clone(),
    ];

    if t.is_unowned() && !t.state.is_terminal() {
        match (&t.last_owner, t.orphaned) {
            (Some(prev), true) => l.push(format!(
                "⤺ UNOWNED — {prev} ended while holding this. Open to rescue: \
                 `paos task claim {}`", t.id)),
            (Some(prev), false) => l.push(format!("⤺ UNOWNED — released by {prev}.")),
            (None, _) => {}
        }
    } else if let Some(who) = &t.claimed_by {
        l.push(format!("held by {who}"));
    }

    let mut meta = vec![format!("scope {}", t.scope)];
    if let Some(r) = &t.repo { meta.push(format!("repo {r}")); }
    if let Some(p) = &t.parent_id { meta.push(format!("epic {p}")); }
    if let Some(r) = &t.room { meta.push(format!("room {r}")); }
    meta.push(format!("created by {} ({})", t.created_by, t.origin.as_str()));
    if t.close_grant { meta.push("sessions may close this".into()); }
    l.push(meta.join(" · "));

    if let Some(b) = &t.body {
        l.push(String::new());
        l.push(b.clone());
    }
    if let Ok(ds) = query::deps(&conn, id) {
        if !ds.is_empty() {
            l.push(String::new());
            let open = ds.iter().filter(|(_, s, _)| !s.is_terminal()).count();
            l.push(if open > 0 {
                format!("depends on ({open} still open — this is blocked):")
            } else {
                "depends on (all clear):".into()
            });
            for (did, dstate, dtitle) in ds {
                l.push(format!("  {did} [{}] {dtitle}", dstate.as_str()));
            }
        }
    }
    if let Ok(ns) = query::notes(&conn, id) {
        if !ns.is_empty() {
            l.push(String::new());
            l.push("log:".into());
            for n in ns {
                l.push(format!("  {}  {:<12} {}", n.ts, n.author, n.text));
            }
        }
    }
    Response::Ok { lines: l }
}

/// Claim, then find out whether it actually worked.
///
/// A spooled write returns "spooled" and nothing more. For every other verb that is
/// fine. For `claim` it is a correctness hole: two sessions spool a claim for the same
/// task, both are told "spooled", and both proceed believing they own it. The daemon
/// applies them in order and the second changes nothing — but nobody told the loser.
///
/// So: send (or spool), then poll the database read-only until the row resolves, and
/// report what the daemon actually decided. Atomicity still lives in the single-writer
/// UPDATE; this only waits to be told the answer. On timeout it says UNCONFIRMED and
/// never "claimed", because a session that starts work on a task it does not own is the
/// exact failure this function exists to prevent.
fn claim_and_confirm(id: &str, me: &str) -> Response {
    let first = write(Request::TaskClaim {
        id: id.into(),
        session: me.into(),
    });
    match &first {
        // A live daemon already answered definitively, either way.
        Response::Err { .. } => return first,
        Response::Ok { lines } if lines.iter().any(|l| l.starts_with("claimed ")) => {
            return first
        }
        _ => {}
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let Some(conn) = paos_bus::readonly::open_ro(&paos_store::db_path()) else { continue };
        let Ok(Some(t)) = store::get(&conn, id) else { continue };
        match t.claimed_by.as_deref() {
            Some(h) if h == me => {
                return Response::Ok {
                    lines: vec![
                        format!("✓ claimed {id} — you own it"),
                        format!("read the briefing first: `paos task show {id}`"),
                    ],
                }
            }
            Some(h) => {
                return Response::err(
                    format!("✗ lost the race — {id} is held by {h}. Pick another with \
                             `paos task ready`."),
                    1,
                )
            }
            None => continue,
        }
    }
    Response::err(
        format!(
            "claim on {id} is UNCONFIRMED — paosd has not applied it yet. Do NOT start \
             work; re-check with `paos task show {id}`."
        ),
        1,
    )
}

/// The repo this session is working in, as `tasks.repo` records it.
fn current_repo() -> Option<String> {
    crate::git_origin()
        .as_deref()
        .and_then(paos_memory::scope::parse_origin)
        .map(|o| o.repo)
}

fn json_tasks(ts: &[paos_tasks::model::Task]) -> String {
    let items: Vec<String> = ts
        .iter()
        .map(|t| {
            format!(
                r#"{{"id":{},"title":{},"state":{},"priority":{},"scope":{},"repo":{},"claimed_by":{},"orphaned":{}}}"#,
                jstr(&t.id),
                jstr(&t.title),
                jstr(t.state.as_str()),
                t.priority,
                jstr(&t.scope),
                t.repo.as_deref().map(jstr).unwrap_or_else(|| "null".into()),
                t.claimed_by.as_deref().map(jstr).unwrap_or_else(|| "null".into()),
                t.orphaned,
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

fn jstr(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_scope_without_a_git_origin_is_an_error_not_a_silent_global() {
        let r = resolve_scope(Some("project"), None);
        assert!(r.is_err(), "must refuse rather than default to global");
        assert!(r.unwrap_err().contains("origin"));
    }

    #[test]
    fn org_scope_without_a_git_origin_is_also_an_error() {
        assert!(resolve_scope(Some("org"), None).is_err());
    }

    /// Owner and repo come back slugged (lowercased, non-alphanumerics folded to `_`),
    /// because that is what memory does to build `org_examplecorp_memory`. Tasks reuse the
    /// same derivation deliberately: two spellings of one owner would put a task in a
    /// scope no recall or filter would ever match.
    #[test]
    fn no_scope_flag_defaults_to_project_inside_a_repo() {
        let (scope, org, repo) =
            resolve_scope(None, Some("git@github.com:ExampleCorp/dotfiles.git")).unwrap();
        assert_eq!(scope, "project");
        assert_eq!(org.as_deref(), Some("examplecorp"));
        assert_eq!(repo.as_deref(), Some("dotfiles"));
    }

    #[test]
    fn no_scope_flag_outside_a_repo_defaults_to_global() {
        let (scope, org, repo) = resolve_scope(None, None).unwrap();
        assert_eq!(scope, "global");
        assert!(org.is_none() && repo.is_none());
    }

    #[test]
    fn org_scope_keeps_the_owner_and_drops_the_repo() {
        let (scope, org, repo) =
            resolve_scope(Some("org"), Some("git@github.com:ExampleCorp/x.git")).unwrap();
        assert_eq!(scope, "org");
        assert_eq!(org.as_deref(), Some("examplecorp"));
        assert!(repo.is_none(), "an org task is not about one repo");
    }

    #[test]
    fn global_scope_ignores_the_repo_it_was_run_in() {
        let (scope, org, repo) =
            resolve_scope(Some("global"), Some("git@github.com:ExampleCorp/x.git")).unwrap();
        assert_eq!(scope, "global");
        assert!(org.is_none() && repo.is_none());
    }

    #[test]
    fn an_unknown_scope_names_the_valid_ones() {
        let e = resolve_scope(Some("team"), None).unwrap_err();
        assert!(e.contains("global|org|project"), "got: {e}");
    }

    /// Each op name must have a matching arm in `paosd::dream::apply_bus_op`. There is no
    /// compiler link between the two, and a missing arm quarantines the write as
    /// malformed — which from here is indistinguishable from success.
    #[test]
    fn every_task_verb_has_a_spool_payload() {
        let reqs = [
            Request::TaskCreate {
                title: "t".into(), body: None, scope: "global".into(), org: None, repo: None,
                parent_id: None, priority: 2, origin: "session".into(),
                created_by: "x".into(), room: None, start_ready: false },
            Request::TaskClaim { id: "t-a".into(), session: "x".into() },
            Request::TaskRelease { id: "t-a".into(), session: "x".into() },
            Request::TaskState { id: "t-a".into(), to: "done".into(), actor: "x".into() },
            Request::TaskNote { id: "t-a".into(), author: "x".into(), text: "n".into() },
            Request::TaskGrant { id: "t-a".into() },
            Request::TaskDep { id: "t-a".into(), depends_on: "t-b".into(), remove: false },
        ];
        let expected = ["task_create", "task_claim", "task_release", "task_state",
                        "task_note", "task_grant", "task_dep"];
        for (r, want) in reqs.iter().zip(expected) {
            let v = degraded_task(r).expect("every task verb must spool");
            assert_eq!(v.get("op").and_then(|o| o.as_str()), Some(want));
        }
    }
}
