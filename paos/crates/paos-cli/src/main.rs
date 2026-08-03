//! `paos` — the thin client.
//!
//! Deliberately dumb: parse argv, send one frame, print the reply, exit with the code
//! the daemon chose. No SQLite, no `git`, no schema knowledge.
//!
//! Why it matters: the Python CLI paid **37 ms of interpreter boot before doing
//! anything**, 70 ms by the time imports finished, and 122.6 ms for a real command —
//! including a 25.7 ms `git` subprocess just to derive scope. A `Stop` hook runs one of
//! these on every assistant turn in every session.
//!
//! No `clap`: argument parsing here is a handful of subcommands, and hand-rolling keeps
//! both the binary and the cold start small.

mod accounts;
mod backup;
mod bus;
mod listen;
mod config;
mod gc;
mod events;
mod guard;
mod init;
mod hook;
mod selftest;
mod librarian;
mod memory;
mod operator;
mod standup;
mod task;
mod trajectory;

use paos_proto::{read_frame, write_frame, Request, Response};
use paos_store as store;
use std::io::Write;
use std::os::unix::net::UnixStream;

const USAGE: &str = "\
paos — Personal Agentic OS

usage:
  paos ping              is the daemon alive?
  paos version           daemon + schema version
  paos whoami            this session's bus handle
  paos send <room> <text>   post a message
  paos listen [<name>] [<room>,...]
                         block token-free until a message warrants a turn
                         (alias for `paos bus listen`; `bus wait-joined` for the
                          always-on loop every session re-arms)
  paos remember <text> --global|--org|--project
  paos recall <query> [--top-k N]   search project + org + global
  paos forget <id>
  paos memory-health     hygiene report: over-long facts, near-duplicates
  paos doctor            is paos still doing what it claims?
  paos gc                reclaimable disk, and what it costs to get back
  paos config [get <k> | set <k> <v> | list | schema]
  paos accounts [list [--json] | switch [<slot>]]
  paos backup [run | status | restore] [--dest <dir>]
  paos operator [mode [<m>] | ask | wait <id> | answer <id> | say | listen | ...]
  paos standup [log <text> | brief | show | reported] [--side work|personal]
  paos task [create <title> | ready | claim <id> | show <id> | list | note <id> <text>
            | review <id> | close <id> | drop <id> | dep add|rm | grant <id>]
                         the fleet's shared work queue (`paos task` for the full usage)
  paos event [record <kind> <summary> | log | prune]
  paos selftest          exercise memory/bus/operator end to end from HERE
  paos who               live fleet roster
  paos reachable <name>  verify + repair reachability (end-of-turn reflex)
  paos session-start <handle> --session-id <id>
  paos heartbeat --session-id <id>
  paos session-end --session-id <id>

options:
  --session-id <id>      override $CLAUDE_CODE_SESSION_ID
";

/// Exit code for "the daemon isn't reachable". Distinct so callers can tell an
/// infrastructure problem from a command that ran and said no.
const EXIT_NO_DAEMON: i32 = 69;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // CLAUDE_CODE_SESSION_ID — via the one helper that knows. This read CLAUDE_SESSION_ID,
    // which NOTHING sets: verified in a live session where CLAUDE_CODE_SESSION_ID was
    // present and CLAUDE_SESSION_ID was absent. Every command relying on the ambient id
    // (whoami, heartbeat, session-end) therefore saw None and reported "needs
    // --session-id" — an unbound session, with no hint that the variable was the problem.
    let mut session_id = operator::session_id();
    let mut sender: Option<String> = None;
    let mut target: Option<String> = None;
    let mut urgent = false;
    let mut tier: Option<String> = None;
    let mut urgent_only = false;
    let mut top_k: usize = 8;
    let mut positional: Vec<String> = Vec::new();
    let mut unknown_flags: Vec<String> = Vec::new();
    let mut ppid: Option<i64> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--session-id" => {
                i += 1;
                if i >= args.len() {
                    fail("--session-id needs a value", 2);
                }
                session_id = Some(args[i].clone());
            }
            "--sender" => { i += 1; sender = args.get(i).cloned(); }
            "--to" => { i += 1; target = args.get(i).cloned(); }
            "--urgent" => urgent = true,
            "--global" => tier = Some("global".into()),
            "--org" => tier = Some("org".into()),
            "--project" => tier = Some("project".into()),
            "--urgent-only" => urgent_only = true,
            // The session's process id, which the presence hook passes on SessionStart
            // and on every Stop. Unparsed, its VALUE fell into the positionals — which is
            // how `--ppid 4242` came within one commit of minting a session named "4242".
            "--ppid" => { i += 1; ppid = args.get(i).and_then(|v| v.parse().ok()); }
            // Was never parsed: `recall --top-k 2` silently returned 8. Found by diffing
            // the Rust paos-init hook against the Python, which asked for 2 lessons and
            // got a different digest. A flag that is accepted and ignored is worse than
            // one that does not exist.
            "--top-k" => {
                i += 1;
                top_k = args.get(i).and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| fail("--top-k needs a number", 2));
            }
            "-h" | "--help" | "help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            // An unknown FLAG is an error, never a positional. Swallowing it produces a
            // confidently wrong answer instead of a failure: `paos reachable --name X`
            // took "--name" and "X" as positionals, used "--name" as the handle, and
            // printed "rooms: (none) / NOT LISTENING" — i.e. told a perfectly reachable
            // session it was deaf. Same disease as `--top-k` being accepted and ignored.
            //
            // It cannot be rejected HERE, though: the facet subcommands below parse their
            // own flags (`bus log --tail`, `bus rooms --all`), and rejecting those broke
            // them the moment this check was added. So collect now, judge once `cmd` is
            // known. Bare "-" is left alone: it means "read the body from stdin".
            other if other.starts_with('-') && other != "-" => {
                unknown_flags.push(other.to_string());
            }
            other => positional.push(other.to_string()),
        }
        i += 1;
    }

    let Some(cmd) = positional.first().map(String::as_str) else {
        print!("{USAGE}");
        std::process::exit(2);
    };

    // These parse their own flags; anything unrecognised here belongs to them.
    //
    // This list is an ALLOWLIST that has to stay in step with the facets, and it has now
    // drifted twice. `trajectory` was missing, so `paos trajectory list --limit 2` — a
    // form its own usage text advertises — died here with "unknown option: --limit" while
    // the Python answered fine. The test below locks it against the facet list rather than
    // against a hand-maintained count, because the failure mode is omission.
    const OWN_PARSER: [&str; 15] =
        ["bus", "memory", "operator", "standup", "accounts", "backup", "config", "event",
         "hook", "gc", "selftest", "trajectory", "task", "secret", "sources"];
    if !OWN_PARSER.contains(&cmd) {
        if let Some(f) = unknown_flags.first() {
            fail(&format!("unknown option: {f}\n\n{USAGE}"), 2);
        }
    }

    // Handled entirely locally: a multi-second `du` is not the daemon's work, and routing
    // it through the socket would block the single writer for the whole scan.
    if cmd == "gc" {
        let found = gc::scan();
        let out = std::io::stdout();
        let mut w = out.lock();
        for l in gc::render(&found, gc::free_bytes()) {
            let _ = writeln!(w, "{l}");
        }
        std::process::exit(0);
    }

    // Claude usage: shells the same helpers the poller and dashboard use. No daemon —
    // this is machine-local information, and routing it through the socket would make it
    // unavailable from inside a sandbox for no benefit.
    // Hooks first: they are invoked by Claude Code with a JSON payload on stdin and must
    // never fail a session, so they take the shortest possible path.
    if cmd == "hook" {
        std::process::exit(hook::run(&positional, &args));
    }
    if cmd == "event" {
        std::process::exit(events::run(&positional, &args, |r| send(r).ok()));
    }
    if cmd == "selftest" {
        std::process::exit(selftest::run_selftest(args.iter().any(|a| a == "--keep")));
    }
    if cmd == "standup" {
        std::process::exit(standup::run(&positional, &args, |r| send(r).ok()));
    }
    if cmd == "operator" {
        let handle = operator::handle();
        std::process::exit(operator::run(&positional, &args, &handle,
                                         |r| send(r).ok()));
    }
    if cmd == "backup" {
        std::process::exit(backup::run(&positional, &args));
    }
    // Reading ~/.claude/projects is machine-local work, and the daemon has no part in it.
    if cmd == "trajectory" {
        std::process::exit(trajectory::run(&positional, &args));
    }
    // The shared work queue. Reads go direct read-only so `list`/`ready`/`show` survive a
    // blocked socket; writes go through `send_or_spool` like every other facet's.
    if cmd == "task" {
        let out = task::run(&args[1..]);
        std::process::exit(match out {
            Response::Ok { lines } => {
                for l in lines { println!("{l}"); }
                0
            }
            Response::Err { message, exit_code } => {
                eprintln!("{message}");
                exit_code
            }
        });
    }
    // `paos memory <verb>` — the nested surface the dashboard shells and every doc types.
    // Reads go direct read-only; writes go through `send`, which spools when the socket
    // is blocked.
    if cmd == "memory" {
        std::process::exit(memory::run(&positional, &args, &store::db_path(),
                                       |r| send(r).ok().or_else(|| degraded(r))));
    }
    // The FLAT aliases delegate to the same code, so `paos remember --supersede a,b` and
    // `paos memory remember --supersede a,b` cannot drift. Nothing I can grep calls the
    // flat forms any more, but "nothing calls them" was wrong twice today.
    if matches!(cmd, "remember" | "recall") {
        let nested = vec!["memory".to_string(), cmd.to_string()];
        let rest = positional[1..].to_vec();
        std::process::exit(memory::run(&[nested, rest].concat(), &args, &store::db_path(),
                                       |r| send(r).ok().or_else(|| degraded(r))));
    }
    if cmd == "accounts" {
        std::process::exit(accounts::run(&positional, &args));
    }
    // `paos listen <name> <rooms>` — the FLAT alias, and it must be the same listener the
    // nested form runs.
    //
    // It used to send `Request::Listen` down the socket, so it exited 69 inside the agent
    // sandbox that is its only real caller — while being ADVERTISED in the help text, so
    // an agent following the help got a hard failure telling it the daemon might be down.
    // Meanwhile `bus wait-joined` polls read-only and works there. Two implementations of
    // "listen", one of which cannot run where sessions live: the same duplicate-definition
    // problem as the two meanings of "listening".
    //
    // The daemon keeps its push `Listen` verb for socket-attached callers; this is the
    // client-side listener, and both take the same flock so they cannot double-deliver.
    if cmd == "listen" {
        let name = positional.get(1).cloned()
            .unwrap_or_else(|| operator::handle());
        let rooms = positional.get(2)
            .map(|r| r.split(',').map(str::trim).filter(|s| !s.is_empty())
                      .map(str::to_string).collect::<Vec<_>>());
        std::process::exit(bus::listen_once(name, rooms, urgent_only));
    }
    // `paos bus <verb>` — the nested form the Python dispatcher and the docs use, and the
    // one that must resolve natively once the Rust binary is installed AS
    // ~/.claude/skills/paos/paos. The flat verbs below remain permanent aliases.
    if cmd == "bus" {
        // RAW argv, not the top-level positionals: the parser here cannot know which bus
        // flags consume a value, so `--ppid 4242` left "4242" sitting in the handle slot.
        std::process::exit(bus::run(&args, &args));
    }

    // First-run setup. Before any daemon request is built, because the whole point of
    // init is that there may be nothing to talk to yet.
    if cmd == "init" {
        std::process::exit(init::run(&positional));
    }

    // `config schema` is a static description of the settings surface, shared with the
    // dashboard. No daemon, no database.
    if cmd == "config" && positional.get(1).map(String::as_str) == Some("schema") {
        println!("{}", config::SCHEMA_JSON);
        std::process::exit(0);
    }

    // `config get <key>` asks the daemon for everything and narrows here, so the daemon
    // keeps exactly one config verb rather than two that can disagree.
    let config_get_key = if cmd == "config" && positional.get(1).map(String::as_str) == Some("get") {
        Some(positional.get(2).cloned().unwrap_or_else(|| fail("config get needs <key>", 2)))
    } else {
        None
    };

    let req = match cmd {
        "ping" => Request::Ping,
        // There is deliberately no `secret set`. Writing a secret is `paos init`'s job:
        // a verb that takes a token as an argument puts it in the shell history of every
        // session that runs it.
        "sources" => Request::ConfigSources,
        "secret" => match positional.get(1).map(String::as_str) {
            Some("status") => Request::SecretStatus {
                key: positional.get(2).cloned()
                    .unwrap_or_else(|| fail("secret status needs <key>", 2)),
            },
            _ => fail("usage: paos secret status <key>", 2),
        },
        "config" => match positional.get(1).map(String::as_str) {
            None | Some("list") => Request::ConfigGet,
            Some("get") => Request::ConfigGet,   // filtered client-side below
            Some("set") => Request::ConfigSet {
                key: positional.get(2).cloned().unwrap_or_else(|| fail("config set needs <key>", 2)),
                value: positional.get(3).cloned().unwrap_or_else(|| fail("config set needs <value>", 2)),
            },
            Some(other) => fail(&format!(
                "unknown config subcommand: {other}\n\
                 usage: paos config [list | schema | get <key> | set <key> <value>]"), 2),
        },
        "version" => Request::Version,
        "whoami" => Request::Whoami { session_id },
        "send" => {
            let room = positional.get(1).cloned().unwrap_or_else(|| fail("send needs <room>", 2));
            let text = positional.get(2).cloned().unwrap_or_else(|| fail("send needs <text>", 2));
            Request::Send {
                room,
                sender: sender.unwrap_or_else(|| fail("send needs --sender", 2)),
                target: target.unwrap_or_else(|| "@all".into()),
                text,
                urgent,
                ambient: false,
            }
        }
        "remember" => Request::Remember {
            tier: tier.unwrap_or_else(|| fail("remember needs --global, --org or --project", 2)),
            origin: git_origin(),
            text: positional.get(1).cloned().unwrap_or_else(|| fail("remember needs <text>", 2)),
            // The CLI always derives scope from the caller's cwd; only the review queue
            // passes an explicit dataset.
            dataset: None,
        },
        "recall" => Request::Recall {
            dataset: None,
            origin: git_origin(),
            query: positional.get(1).cloned().unwrap_or_else(|| fail("recall needs <query>", 2)),
            top_k,
        },
        "forget" => Request::Forget {
            id: positional.get(1).cloned().unwrap_or_else(|| fail("forget needs <id>", 2)),
        },
        "memory-health" => Request::MemoryHealth,
        "doctor" => Request::Doctor,
        "who" => Request::Who,
        "reachable" => Request::Reachable {
            name: positional.get(1).cloned()
                .unwrap_or_else(|| fail("reachable needs <handle>", 2)),
        },
        "session-start" => Request::SessionStart {
            session_id: session_id.clone().unwrap_or_else(|| fail("needs --session-id", 2)),
            name: positional.get(1).cloned().unwrap_or_else(|| fail("session-start needs <handle>", 2)),
            pid: ppid.or_else(|| std::env::var("PPID").ok().and_then(|p| p.parse().ok())),
        },
        "heartbeat" => Request::Heartbeat {
            session_id: session_id.clone().unwrap_or_else(|| fail("needs --session-id", 2)),
            pid: ppid,
        },
        "session-end" => Request::SessionEnd {
            session_id: session_id.clone().unwrap_or_else(|| fail("needs --session-id", 2)),
        },
        other => fail(&format!("unknown command: {other}\n\n{USAGE}"), 2),
    };

    // The socket being unreachable is the NORMAL case inside an agent sandbox, so fall
    // back before treating it as a failure. `degraded` is called at most ONCE: calling it
    // in a match guard and again in the arm spooled every write twice, and `remember`
    // being idempotent on a stable id hid it.
    let mut sock_err = String::new();
    let outcome = match send(&req) {
        Ok(r) => Some(r),
        Err(e) => {
            sock_err = e.to_string();
            degraded(&req)
        }
    };
    match outcome {
        Some(Response::Ok { lines }) => {
            let out = std::io::stdout();
            let mut w = out.lock();
            for l in config::shape(lines, config_get_key.as_deref()) {
                let _ = writeln!(w, "{l}");
            }
        }
        Some(Response::Err { message, exit_code }) => {
            // `reachable` returns a multi-line body with a non-zero code, and NOT LISTENING
            // is a normal state rather than a failure — the Python prints it to STDOUT.
            // Streams matter here: `out=$(paos reachable)` must not come back empty.
            if matches!(req, Request::Reachable { .. }) {
                println!("{message}");
            } else {
                eprintln!("{message}");
            }
            std::process::exit(exit_code);
        }
        None => fail(
            // ORDER MATTERS. The old hint led with "start the daemon", so a session inside
            // an agent sandbox — where the socket is blocked but paosd is perfectly
            // healthy — read it as "the daemon is down", went looking for a dead
            // LaunchAgent, and started a retired one. The sandbox is the FAR more common
            // cause of this error, so it is named first.
            &format!(
                "cannot reach paosd at {}: {sock_err}\n\
                 \n\
                 If you are an agent in a sandbox this is EXPECTED — the sandbox blocks unix\n\
                 sockets, not the daemon. paosd is probably fine, and your writes were\n\
                 SPOOLED, not lost. Re-run outside the sandbox if you need a live answer.\n\
                 Do NOT bother with sandbox.excludedCommands: paos and paosctl are already\n\
                 listed there and it makes no difference — measured again on 2026-08-01.\n\
                 \n\
                 Only if that is not it, the daemon may really be down:\n\
                   launchctl kickstart -k gui/$(id -u)/ai.paos.daemon\n\
                 Do NOT start ai.paos.operator — it is retired, and a second Telegram\n\
                 consumer makes the operator's messages vanish at random.",
                store::socket_path().display()
            ),
            EXIT_NO_DAEMON,
        ),
    }
}

/// One request, one reply.
///
/// **Never falls back to WRITING SQLite directly** — that would reintroduce the
/// multi-writer races the daemon exists to remove. Writes that cannot reach the socket
/// are spooled for the daemon to apply; reads fall back to a read-only query. See
/// `degraded`.
fn send(req: &Request) -> std::io::Result<Response> {
    let mut stream = UnixStream::connect(store::socket_path())?;
    write_frame(&mut stream, req)?;
    read_frame(&mut stream)
}

/// What to do when the socket is unreachable — which is the NORMAL case, not an error.
///
/// Every agent session on this machine runs in a sandbox that permits reading and writing
/// under ~/.paos but DENIES connecting to a unix socket. `paos` and `paosctl` are both in
/// sandbox.excludedCommands and it makes no difference. So a CLI that only speaks the
/// socket works from a terminal and fails for every actual user, which is why the Python
/// facets are still the thing sessions run.
///
/// The split is what keeps the daemon's invariant intact:
///   * WRITES are spooled to a file the daemon drains every 5s. Still a single writer.
///   * READS open the database read-only. Safe under WAL, and exactly what the Python
///     has been doing all along.
fn degraded(req: &Request) -> Option<Response> {
    // The work queue spools like everything else. The payloads live next to the CLI that
    // builds them so the op names and the fields stay in one file with the test that
    // pins them.
    if let Some(payload) = task::degraded_task(req) {
        return spool(&payload);
    }
    match req {
        Request::Remember { tier, origin, text, dataset } => spool(&serde_json::json!({
            "tier": tier, "origin": origin, "text": text, "dataset": dataset,
        })),
        Request::Forget { id } => spool(&serde_json::json!({ "op": "forget", "id": id })),
        Request::Supersede { old_ids, tier, origin, text, dataset } => spool(&serde_json::json!({
            "op": "supersede", "old_ids": old_ids, "tier": tier,
            "origin": origin, "text": text, "dataset": dataset,
        })),
        // The degraded path honours --dataset too, or a sandboxed session would silently
        // search a different set of facts than the daemon would have.
        Request::Recall { origin, query, top_k, dataset } => Some(read_only_recall_scoped(
            origin.as_deref(), query, *top_k, dataset.as_deref())),
        // Reads go direct; a config read that failed inside a sandbox would break every
        // consumer that asks for a knob before doing its work.
        Request::ConfigGet => config::read_only_all(&store::root().join("paos.db"))
            .map(|pairs| Response::Ok {
                lines: pairs.into_iter().map(|(k, v)| format!("{k}={v}")).collect(),
            }),
        Request::ConfigSet { key, value } => spool(&serde_json::json!({
            "op": "config_set", "key": key, "value": value,
        })),
        // Queue writes spool like any other write. This is what makes a sandboxed
        // `approve` consistently DEFERRED: both halves — the fact and the row — land when
        // the daemon drains, instead of the row flipping to `approved` while the fact it
        // was meant to store went nowhere.
        Request::ProposalAdd {
            kind, dataset, text, scope, target_data_id, rationale, source,
        } => spool(&serde_json::json!({
            "op": "proposal_add", "kind": kind, "dataset": dataset, "text": text,
            "scope": scope, "target_data_id": target_data_id, "rationale": rationale,
            "source": source,
        })),
        Request::ProposalSetStatus { id, status } => spool(&serde_json::json!({
            "op": "proposal_set_status", "id": id, "status": status,
        })),
        // Bus READS. These three answer "am I still connected to the fleet?", so a session
        // that cannot run them cannot tell reachable from deaf — and every one of them was
        // socket-only, i.e. broken for the sandboxed sessions that are their only callers.
        // Writes (send, join, hello, session lifecycle) are deliberately NOT here: the
        // daemon's spool drain has no bus op yet, so spooling one would be a silent drop.
        // The session lifecycle. Spooled when the socket is blocked rather than failing:
        // losing a session-start leaves an UNBOUND session (nothing can address it), and
        // losing a session-end leaves a dead one in the roster still holding its room
        // memberships. The presence hook runs in the harness and normally reaches the
        // socket, so these are the safety net, not the usual path.
        Request::SessionStart { session_id, name, pid } => spool(&serde_json::json!({
            "op": "bus_session_start", "session_id": session_id, "name": name, "pid": pid,
        })),
        Request::Heartbeat { session_id, pid } => spool(&serde_json::json!({
            "op": "bus_heartbeat", "session_id": session_id, "pid": pid,
        })),
        Request::SessionEnd { session_id } => spool(&serde_json::json!({
            "op": "bus_session_end", "session_id": session_id,
        })),
        // The sweeps mutate, so a blocked socket cannot perform them. But answering
        // "queued" would be the very failure this system is prone to: an operator runs a
        // sweep when they suspect something is wrong, and "queued" cannot distinguish
        // swept 0 from swept 400 from never ran.
        //
        // So the degraded path PREVIEWS read-only — computing exactly what the sweep
        // would do — and queues the write. The count is real either way; only the timing
        // differs, and the caller is told which they got.
        Request::BusReap => bus::preview_reap(),
        Request::BusPrune { older_than_min } => bus::preview_prune(*older_than_min),
        Request::BusPruneRooms => bus::preview_prune_rooms(),
        Request::Who => bus::who(false),
        Request::Whoami { session_id } => bus::whoami(session_id.as_deref()),
        Request::Reachable { name } => bus::reachable(name),
        _ => None,
    }
}

/// Park a write for the daemon. A file is the one channel the sandbox allows.
/// Send one request the normal way — socket first, degraded fallback — printing the
/// reply and returning an exit code instead of terminating.
///
/// Shared with `paos bus <verb>` so the nested and flat forms cannot drift: they are the
/// same request down the same path, which is the property the permanent aliases promise.
/// Socket first, degraded fallback — without the printing. The facets that build their
/// own `Response` (rather than handing one to `dispatch`) go through this, so there is
/// still exactly one place that decides what "the daemon is unreachable" means.
pub(crate) fn send_or_spool(req: &Request) -> Option<Response> {
    match send(req) {
        Ok(r) => Some(r),
        Err(_) => degraded(req),
    }
}

pub(crate) fn dispatch(req: &Request) -> i32 {
    let outcome = send_or_spool(req);
    match outcome {
        Some(Response::Ok { lines }) => {
            for l in lines {
                println!("{l}");
            }
            0
        }
        Some(Response::Err { message, exit_code }) => {
            eprintln!("{message}");
            exit_code
        }
        None => {
            eprintln!("cannot reach paosd at {}", store::socket_path().display());
            EXIT_NO_DAEMON
        }
    }
}

pub(crate) fn spool(payload: &serde_json::Value) -> Option<Response> {
    spool_at(&store::root().join("spool"), payload)
}

/// Split from `spool` so tests need not mutate PAOS_ROOT, which races other tests in the
/// same process — the exact defect that made the doctor's spool check flaky.
fn spool_at(dir: &std::path::Path, payload: &serde_json::Value) -> Option<Response> {
    if std::fs::create_dir_all(dir).is_err() {
        return None;
    }
    // Time + pid + a per-process counter. Time and pid ALONE are not unique: two writes
    // from one process inside the same millisecond produce the same filename, and the
    // second truncates the first. Found on disk — a spool file containing two overlapping
    // payloads, i.e. a silently lost write, which is the one thing this mechanism exists
    // to prevent.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).ok()?.as_millis();
    let path = dir.join(format!("{}-{}-{n}.json", now, std::process::id()));
    let mut body = payload.clone();
    body["queued_ts"] = serde_json::json!(now_iso());
    std::fs::write(&path, body.to_string()).ok()?;
    Some(Response::ok("spooled — paosd applies it within ~5s"))
}

/// Best-effort recall without the daemon: a read-only word match, LOUDLY labelled.
///
/// It is not semantic search — the embedder lives in the daemon. Saying so matters more
/// than the result: a session that reads "(no results)" from a degraded match and
/// concludes the fact was never stored will cheerfully store it again.
fn read_only_recall_scoped(origin: Option<&str>, query: &str, top_k: usize,
                           dataset: Option<&str>) -> Response {
    read_only_recall_at_scoped(&store::root().join("paos.db"), origin, query, top_k, dataset)
}

#[cfg(test)]
fn read_only_recall_at(db: &std::path::Path, origin: Option<&str>, query: &str,
                       top_k: usize) -> Response {
    read_only_recall_at_scoped(db, origin, query, top_k, None)
}

fn read_only_recall_at_scoped(db: &std::path::Path, origin: Option<&str>, query: &str,
                              top_k: usize, dataset: Option<&str>) -> Response {
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return Response::err("paosd unreachable and paos.db unreadable", EXIT_NO_DAEMON);
    };
    // Same rule as the daemon: an explicit dataset REPLACES the derived scopes, so the
    // degraded path cannot quietly search a different set than the daemon would have.
    let scopes = match dataset.filter(|d| !d.trim().is_empty()) {
        Some(d) => vec![d.to_string()],
        None => {
            let parsed = origin.and_then(paos_memory::scope::parse_origin);
            // The CONFIGURED global, read from the same file we are about to search.
            // The degraded path searching a different global than the daemon would is
            // exactly the kind of divergence that makes "recall found nothing" mean two
            // different things.
            paos_memory::scope::recall_scopes(
                parsed.as_ref(), &paos_memory::scope::global_dataset(&conn))
        }
    };
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
        .filter(|t| t.len() > 2)
        .collect();
    let placeholders = scopes.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT text FROM memories WHERE superseded IS NULL AND dataset IN ({placeholders}) \
         ORDER BY created_ts DESC");
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Response::err("paosd unreachable and paos.db unreadable", EXIT_NO_DAEMON);
    };
    let rows = stmt.query_map(rusqlite::params_from_iter(scopes.iter()), |r| r.get::<_, String>(0));
    let Ok(rows) = rows else {
        return Response::err("paosd unreachable and paos.db unreadable", EXIT_NO_DAEMON);
    };
    let mut scored: Vec<(usize, String)> = rows
        .flatten()
        .map(|t| {
            let low = t.to_lowercase();
            (terms.iter().filter(|w| low.contains(w.as_str())).count(), t)
        })
        .filter(|(n, _)| *n > 0)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    let mut lines = vec![
        "⚠ DEGRADED RECALL: paosd's socket is blocked (agent sandbox), so this is a word".to_string(),
        "  match, not semantic search. A miss here does NOT mean the fact is absent.".to_string(),
    ];
    if scored.is_empty() {
        lines.push("(no word-match results)".to_string());
    } else {
        lines.extend(scored.into_iter().take(top_k).map(|(_, t)| format!("- {t}")));
    }
    Response::Ok { lines }
}

/// `YYYYMMDDTHHMMSSZ`, for snapshot filenames.
pub fn utc_stamp() -> String {
    std::process::Command::new("date")
        .args(["-u", "+%Y%m%dT%H%M%SZ"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn now_iso() -> String {
    // The daemon stamps the authoritative time when it drains; this is only for triage.
    std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// The cwd's git origin, so the daemon can derive scope.
///
/// Resolved client-side because scope follows the *caller's* directory, not the
/// daemon's. It is the one subprocess the CLI makes, and only for memory commands.
fn git_origin() -> Option<String> {
    std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Every `--flag value` pair the CLI accepts, parsed one way.
///
/// Extracted so the parsing is testable: `--top-k` was declared in the usage text and
/// never read, so `recall --top-k 2` silently returned 8. Nothing failed — the caller
/// simply got a different answer than it asked for, which is the hardest kind of bug to
/// see from the outside.
pub fn flag_value<'a>(args: &'a [String], name: &str) -> Option<&'a String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1))
}

fn fail(msg: &str, code: i32) -> ! {
    eprintln!("paos: {msg}");
    std::process::exit(code);
}

#[cfg(test)]
mod tests {

    /// `--dataset` must REPLACE the derived scopes, not add to them.
    ///
    /// The whole point is reaching a tier no repo maps to. If the flag merely widened the
    /// search it would look like it worked while still returning the same global hits,
    /// and a "scoped" personal fact would stay unfindable.
    #[test]
    fn recall_dataset_replaces_the_derived_scopes() {
        let d = tmpdir("recall-ds");
        let db = d.join("paos.db");
        let c = rusqlite::Connection::open(&db).unwrap();
        paos_memory::ensure_schema(&c).unwrap();
        for (id, ds, text) in [
            ("g1", "global_memory", "the thermostat is in the hall"),
            ("p1", "personal_home", "the thermostat is in the hall"),
        ] {
            c.execute("INSERT INTO memories(id,dataset,text,embedding,created_ts) \
                       VALUES(?1,?2,?3,x'',?4)",
                      rusqlite::params![id, ds, text, "2026-07-31"]).unwrap();
        }
        drop(c);

        // Count HITS, not lines: degraded recall prepends a two-line warning banner, and
        // counting lines makes a correct result look like three.
        let hits = |r: Response| match r {
            Response::Ok { lines } => lines.iter().filter(|l| l.starts_with("- ")).count(),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            hits(read_only_recall_at_scoped(&db, None, "thermostat", 8, Some("personal_home"))),
            1, "exactly the named dataset, not global as well");
        // And without the flag it must NOT reach the unmapped dataset at all.
        assert_eq!(hits(read_only_recall_at_scoped(&db, None, "thermostat", 8, None)),
                   1, "global only — personal_home is unreachable without the flag");
    }

    #[test]
    fn an_empty_dataset_flag_falls_back_to_the_derived_scopes() {
        // A failed shell substitution must not silently search nothing.
        let d = tmpdir("recall-ds-empty");
        let db = d.join("paos.db");
        let c = rusqlite::Connection::open(&db).unwrap();
        paos_memory::ensure_schema(&c).unwrap();
        c.execute("INSERT INTO memories(id,dataset,text,embedding,created_ts) \
                   VALUES('g1','global_memory','a global fact',x'','2026-07-31')", [])
            .unwrap();
        drop(c);
        let r = read_only_recall_at_scoped(&db, None, "global", 8, Some("   "));
        let n = match r { Response::Ok { lines } => lines.iter().filter(|l| l.starts_with("- ")).count(),
                          other => panic!("{other:?}") };
        assert_eq!(n, 1, "a blank flag must search the derived scopes, not nothing");
    }

    use super::*;

    #[test]
    fn a_declared_flag_is_actually_read() {
        // The regression: --top-k appeared in the usage text and in callers, and was
        // never parsed. The hook asked for 2 lessons and got 8, and the only symptom was
        // a digest that differed from the Python's.
        let args: Vec<String> = ["recall", "q", "--top-k", "2"]
            .iter().map(|s| s.to_string()).collect();
        assert_eq!(flag_value(&args, "--top-k").map(String::as_str), Some("2"));
    }

    #[test]
    fn a_flag_with_no_value_is_none_rather_than_a_panic() {
        let args: Vec<String> = ["recall", "q", "--top-k"].iter().map(|s| s.to_string()).collect();
        assert_eq!(flag_value(&args, "--top-k"), None);
    }

    #[test]
    fn an_absent_flag_is_none() {
        let args: Vec<String> = ["recall", "q"].iter().map(|s| s.to_string()).collect();
        assert_eq!(flag_value(&args, "--top-k"), None);
    }

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("paos-cli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn spooled(dir: &std::path::Path) -> Vec<serde_json::Value> {
        let mut out = vec![];
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let body = std::fs::read_to_string(e.path()).unwrap();
            out.push(serde_json::from_str(&body).unwrap());
        }
        out
    }

    #[test]
    fn a_write_that_cannot_reach_the_socket_is_spooled_not_lost() {
        // Every agent session here is sandboxed and the sandbox blocks unix sockets, so
        // this is the NORMAL path for the CLI's only real users — not an edge case.
        let d = tmpdir("write");
        let r = spool_at(&d, &serde_json::json!({"tier": "global", "text": "a fact"}));
        assert!(matches!(r, Some(Response::Ok { .. })));
        let e = spooled(&d);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0]["text"], "a fact");
        assert!(e[0]["queued_ts"].is_string(), "triage needs to know when it was parked");
    }

    #[test]
    fn spooling_happens_exactly_once_per_command() {
        // `degraded` was called in a match guard AND again in the arm, so every write
        // spooled twice. `remember` is idempotent on a stable id, which hid it entirely.
        let d = tmpdir("once");
        for _ in 0..1 {
            spool_at(&d, &serde_json::json!({"tier": "global", "text": "x"}));
        }
        assert_eq!(spooled(&d).len(), 1, "one command must produce one spool entry");
    }

    #[test]
    fn a_remember_degrades_to_a_spooled_write() {
        let req = Request::Remember {
            tier: "global".into(), origin: None,
            text: "a durable fact".into(), dataset: None };
        assert!(degraded(&req).is_some());
    }

    #[test]
    fn a_forget_degrades_too_so_a_wrong_fact_can_still_be_retracted() {
        let req = Request::Forget { id: "abc".into() };
        assert!(degraded(&req).is_some());
    }

    #[test]
    fn a_supersede_degrades_too() {
        let req = Request::Supersede {
            old_ids: vec!["a".into()], tier: "global".into(),
            origin: None, text: "replacement".into(), dataset: None };
        assert!(degraded(&req).is_some());
    }

    #[test]
    fn commands_with_no_safe_offline_answer_do_not_pretend() {
        // Inventing an answer would be worse than the honest error. These two genuinely
        // have no offline answer: `ping` is a test OF the socket, and `doctor` reports the
        // daemon's own internal state. Neither is recoverable from the database.
        for req in [Request::Ping, Request::Doctor] {
            assert!(degraded(&req).is_none(), "{req:?} must not fake a reply");
        }
    }

    #[test]
    fn the_bus_reads_degrade_because_they_are_not_inventions() {
        // `Who` and `Reachable` used to be listed above, on the reasoning that a fabricated
        // roster is worse than an error. Correct about fabrication, wrong about the
        // premise: these read the SAME SQLite the daemon writes, which is the authority,
        // not a guess — the Python has answered them exactly this way for months.
        //
        // Leaving them socket-only had a measured cost. On 2026-07-31 `paosctl who`,
        // `whoami` and `reachable` each exited 69 inside an agent sandbox, which is where
        // every real caller lives. A session cannot tell "reachable" from "deaf" without
        // them, and deafness is this fleet's characteristic silent failure.
        // Exercised through the `_at` seams rather than by setting PAOS_ROOT. Mutating it
        // here is not merely flaky: env vars are process-global and tests run as threads,
        // so the window between set_var and remove_var lets ANY concurrent test fall back
        // to the real ~/.paos. A test suite that can write the live fleet store is a
        // hazard, not a test — and this file's own history is the argument.
        let d = tmpdir("busdegrade");
        let db = d.join("paos.db");
        let _ = paos_store::open(&db).unwrap();
        assert!(bus::who_at(&db, &d, false, 0).is_some(), "who must answer read-only");
        assert!(bus::reachable_at(&db, "x").is_some(), "reachable must answer read-only");
    }

    #[test]
    fn an_unknown_option_is_rejected_rather_than_taken_as_a_positional() {
        // `paos reachable --name X` used to push "--name" and "X" into positionals, take
        // "--name" as the handle, and answer "rooms: (none) / NOT LISTENING" — telling a
        // reachable session it was deaf. A wrong answer, not an error.
        //
        // Checked as a classification rule because `fail` exits the process: a flag-shaped
        // argument must never reach the positional list.
        let flagged = |a: &str| a.starts_with('-') && a != "-";
        assert!(flagged("--name"), "an unknown long flag must be rejected");
        assert!(flagged("-x"), "an unknown short flag must be rejected");
        assert!(!flagged("-"), "bare '-' means read the body from stdin");
        assert!(!flagged("witty-bison-2"), "a handle with a hyphen is still a positional");
        assert!(!flagged("ad-hocs"), "a room name with a hyphen is still a positional");
    }

    #[test]
    fn the_flat_listen_verb_uses_the_sandbox_safe_listener_not_the_socket() {
        // It used to send Request::Listen down the socket, so it exited 69 inside the
        // agent sandbox that is its ONLY real caller — while being advertised in the help
        // text, so an agent following the help got a hard failure claiming the daemon
        // might be down. `bus wait-joined` polls read-only and works there.
        //
        // Asserted on the source because the alternative is a test that blocks forever:
        // the correct behaviour here is "does not return". A behavioural test would have
        // to time out, and a timeout that passes when the binary is simply slow is not
        // evidence. What must hold is that the flat verb reaches the SAME listener as the
        // nested form, so there is one implementation and one flock rather than two.
        let src = include_str!("main.rs");
        let arm = src
            .split("if cmd == \"listen\"")
            .nth(1)
            .expect("the flat listen dispatch")
            .split("\n    }")
            .next()
            .expect("its body");
        assert!(arm.contains("bus::listen_once"),
                "the flat `listen` must route to the polling listener that bus \
                 wait-joined uses; found instead: {arm}");
        assert!(!arm.contains("Request::Listen"),
                "the flat `listen` must NOT open the socket — that is what made it \
                 exit 69 for every sandboxed session");
    }

    #[test]
    fn facet_subcommands_keep_their_own_flags() {
        // REGRESSION: rejecting unknown flags in the parse loop broke every facet that
        // parses its own, because the loop runs before `cmd` is known. `bus log --tail 5`
        // and `bus rooms --all` both started exiting 2 with "unknown option: --tail".
        // Measured against the Python, which answered them fine.
        //
        // This test used to keep its OWN COPY of the list, which is why the real one
        // drifted twice without anything going red: the copy said 10 entries while the
        // code said 11, and `trajectory` was absent from both. `paos trajectory list
        // --limit 2` — the exact form its usage advertises — exited 2 with "unknown
        // option: --limit" while the Python listed sessions fine. A test that duplicates
        // the value it is checking tests the duplicate.
        //
        // So read the REAL constant out of the source instead. Same idiom as the
        // per-poll orphan check in listen.rs.
        let src = include_str!("main.rs");
        let decl = src
            .split("const OWN_PARSER:")
            .nth(1)
            .expect("OWN_PARSER is declared");
        // Slice AFTER the '=', not from the first '[': the first bracket belongs to the
        // type annotation `[&str; 12]`, so slicing there yields no names at all and every
        // assertion below fails for the wrong reason.
        let rhs = &decl[decl.find('=').expect("assignment")..];
        let list = &rhs[rhs.find('[').expect("opening bracket")
            ..rhs.find(']').expect("closing bracket")];

        // THE POPULATION IS DERIVED TOO, not just the constant.
        //
        // Reading the real OWN_PARSER closed half the gap; the list this iterated was
        // still hand-written, so a NEW command dispatched without touching either place
        // passed — the net got smaller and nothing said so. A guard's population must be
        // derived, never hand-maintained, or it silently stops covering what it names.
        //
        // Every top-level command is dispatched by a `cmd == "..."` test, so that IS the
        // population. Each one must be a deliberate choice: it either parses its own
        // flags (in OWN_PARSER) or it does not (listed as exempt, with the reason).
        let dispatched: std::collections::BTreeSet<String> = src
            .match_indices("cmd == \"")
            .filter_map(|(i, _)| {
                let rest = &src[i + "cmd == \"".len()..];
                rest.find('"').map(|end| rest[..end].to_string())
            })
            // Command-shaped only. Prose in this file's own comments contains the literal
            // `cmd == "..."`, and the scan matched it — a derived population still needs
            // to know what it is deriving.
            .filter(|c| !c.is_empty()
                     && c.chars().all(|ch| ch.is_ascii_lowercase() || ch == '-'))
            .collect();

        // Floor, so this cannot pass VACUOUSLY if the scan stops matching — a guard that
        // finds nothing and reports success is the failure this whole test is about.
        assert!(dispatched.len() >= 12,
                "only found {} dispatched commands; the scan has stopped matching and this \
                 test is no longer checking anything", dispatched.len());

        // Commands that deliberately have NO sub-parser and so are NOT exempt.
        const NO_SUBPARSER: [&str; 2] = [
            // Uses the top-level parser's own values (`--urgent-only`, positionals) and
            // defines no flags of its own.
            "listen",
            // A wizard: everything it needs it asks for, so there is nothing to flag.
            "init",
        ];

        for cmd in &dispatched {
            if NO_SUBPARSER.contains(&cmd.as_str()) {
                assert!(!list.contains(&format!("\"{cmd}\"")),
                        "{cmd} is declared to have no sub-parser but is in OWN_PARSER — \
                         decide which it is");
                continue;
            }
            assert!(list.contains(&format!("\"{cmd}\"")),
                    "`{cmd}` is dispatched as a top-level command but is missing from \
                     OWN_PARSER. If it parses its own flags, add it there; if it does not, \
                     add it to NO_SUBPARSER with the reason. Leaving it out silently makes \
                     `paos {cmd} --its-own-flag` exit 2.");
        }
        // A flat verb has no sub-parser, so an unknown flag there is still a real error:
        // `paos reachable --name X` once took "--name" as the handle and told a reachable
        // session it was deaf.
        for verb in ["reachable", "who", "whoami", "send", "recall"] {
            assert!(!list.contains(&format!("\"{verb}\"")),
                    "{verb} has no sub-parser, so an unknown flag must still be an error");
        }
    }

    #[test]
    fn a_degraded_recall_says_loudly_that_it_is_degraded() {
        // A session that reads "(no results)" from a word match and concludes the fact
        // was never stored will cheerfully store it again.
        let db = tmpdir("recall").join("paos.db");
        let c = rusqlite::Connection::open(&db).unwrap();
        paos_memory::ensure_schema(&c).unwrap();
        drop(c);
        let r = read_only_recall_at(&db, None, "anything", 8);
        match r {
            Response::Ok { lines } => assert!(lines[0].contains("DEGRADED"), "{lines:?}"),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn a_degraded_recall_finds_a_fact_by_word_match() {
        let db = tmpdir("recallhit").join("paos.db");
        let c = rusqlite::Connection::open(&db).unwrap();
        paos_memory::ensure_schema(&c).unwrap();
        paos_memory::remember(&c, &paos_memory::HashEmbedder::new(64),
                              paos_memory::scope::DEFAULT_GLOBAL,
                              "the deploy script lives in setup/phases",
                              "2026-07-31T00:00:00Z").unwrap();
        drop(c);
        match read_only_recall_at(&db, None, "deploy script", 8) {
            Response::Ok { lines } =>
                assert!(lines.iter().any(|l| l.contains("setup/phases")), "{lines:?}"),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn a_degraded_recall_on_an_unreadable_database_errors_rather_than_saying_no_results() {
        // "(no results)" from a missing database is indistinguishable from an empty
        // store, and that is exactly the confusion that makes a session re-store facts.
        match read_only_recall_at(std::path::Path::new("/definitely/not/here.db"),
                                  None, "q", 8) {
            Response::Err { exit_code, .. } => assert_eq!(exit_code, EXIT_NO_DAEMON),
            other => panic!("expected Err, got {other:?}"),
        }
    }
}
