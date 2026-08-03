//! `paos operator` — the agent side of the human channel.
//!
//! Ported from `operator_facet.py`, which was a second implementation of a DB layer
//! `paos-operator` already had: the daemon used one, sessions used the other, against the
//! same tables. Same shape as the `accounts` duplication, and the reason a threshold or a
//! status string could drift between what a session wrote and what the daemon read.
//!
//! Reads and blocking waits go DIRECT (read-only, safe under WAL, and they must work from
//! inside a sandbox where the socket is blocked). Writes go through the daemon.

use paos_operator as op;
use paos_proto::{Request, Response};

/// How often a blocking wait re-checks. The answer arrives via Telegram, so the operator
/// is minutes away; polling faster buys nothing and spins a CPU on an idle laptop.
const POLL_SECS: u64 = 2;

/// This session's BUS HANDLE — the name escalations and outbox rows are attributed to,
/// and the name the operator sees when answering "#3 [quiet-otter] ...".
///
/// Resolved from the sessions table by CLAUDE_SESSION_ID, exactly as the Python did via
/// `bus_facet.resolved_identity()`. Env-var guesses like CONDUCTOR_WORKSPACE_NAME are NOT
/// the same string, and attributing an escalation to the wrong session means the answer
/// is routed back to the wrong one.
pub fn handle() -> String {
    // 1. Explicit override. Used by paosd to speak as the human; deliberately an env var
    //    and NOT a flag, so an agent cannot rename itself.
    if let Ok(v) = std::env::var("PAOS_IDENTITY") {
        if !v.trim().is_empty() {
            return v.trim().to_string();
        }
    }
    // 2. The bound bus handle. The session id comes from CLAUDE_CODE_SESSION_ID (note:
    //    not CLAUDE_SESSION_ID — a different variable, and using the wrong one silently
    //    yields an unbound session), else the pointer file the hook writes.
    if let Some(sid) = session_id() {
        if let Some(c) = ro() {
            if let Ok(name) = c.query_row(
                "SELECT name FROM sessions WHERE session_id=?1 AND ended_ts IS NULL",
                [&sid], |r| r.get::<_, String>(0)) {
                return name;
            }
        }
    }
    // 3. The legacy identity file, for a session with no bound id.
    if let Ok(v) = std::fs::read_to_string(paos_store::root().join(".identity").join(fs_key())) {
        if !v.trim().is_empty() {
            return v.trim().to_string();
        }
    }
    // Deliberately does NOT mint a new handle. Minting is a write and belongs to the bus;
    // inventing one here would attribute an escalation to a session that does not exist,
    // and the answer could never be routed back.
    "unbound-session".into()
}

/// `CONDUCTOR_WORKSPACE_NAME`, else the workspace path basename, else the cwd basename.
///
/// Shared with `bus rename`, which persists the corrected identity under this key.
pub(crate) fn fs_key() -> String {
    let raw = std::env::var("CONDUCTOR_WORKSPACE_NAME").ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("CONDUCTOR_WORKSPACE_PATH").ok()
            .map(|p| p.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string())
            .filter(|s| !s.is_empty()))
        .or_else(|| std::env::current_dir().ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string())))
        .unwrap_or_default();
    let s: String = raw.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let s = s.trim_matches('.').to_string();
    if s.is_empty() { "anon".into() } else { s }
}

/// This Claude Code session's id. Shared with the bus and the top-level arg parser so
/// there is exactly ONE place that knows which variable carries it.
pub(crate) fn session_id() -> Option<String> {
    if let Ok(v) = std::env::var("CLAUDE_CODE_SESSION_ID") {
        if !v.trim().is_empty() {
            return Some(v.trim().to_string());
        }
    }
    std::fs::read_to_string(paos_store::root().join(".session").join(fs_key()))
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn ro() -> Option<rusqlite::Connection> {
    rusqlite::Connection::open_with_flags(
        paos_store::db_path(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
}

/// Body from argv, or from stdin when it is `-`.
///
/// The stdin form is what makes a multi-line or backtick-bearing body safe: it never
/// passes through the shell.
fn body(arg: Option<&String>) -> String {
    match arg.map(String::as_str) {
        Some("-") | None => {
            use std::io::Read;
            let mut s = String::new();
            let _ = std::io::stdin().read_to_string(&mut s);
            s.trim().to_string()
        }
        Some(t) => t.to_string(),
    }
}

pub fn run(positional: &[String], args: &[String], handle: &str,
           send: impl Fn(&Request) -> Option<Response>) -> i32 {
    let sub = positional.get(1).map(String::as_str).unwrap_or("mode");
    let arg = positional.get(2);
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1));

    match sub {
        // --- reads: no daemon needed -------------------------------------------------
        "mode" | "status" if arg.is_none() => {
            let Some(c) = ro() else { eprintln!("paos.db unreadable"); return 1 };
            println!("{}", op::get_mode(&c).as_str());
            0
        }
        "blocked" => {
            let Some(c) = ro() else { eprintln!("paos.db unreadable"); return 1 };
            let rows = op::open_escalations(&c).unwrap_or_default();
            if rows.is_empty() {
                println!("(no open escalations)");
            } else {
                for (id, s, q) in rows { println!("#{id} [{s}] {q}"); }
            }
            0
        }
        "parked" => {
            let Some(c) = ro() else { eprintln!("paos.db unreadable"); return 1 };
            let rows = op::open_parked(&c).unwrap_or_default();
            if rows.is_empty() {
                println!("(nothing parked)");
            } else {
                for (id, s, n) in rows { println!("#{id} [{s}] {n}"); }
            }
            0
        }
        "wait" => wait_escalation(arg.and_then(|a| a.parse().ok())),
        "listen" => listen(),

        // --- writes: through the daemon ----------------------------------------------
        "mode" | "status" => once(&send, &Request::OperatorSetMode {
            mode: arg.cloned().unwrap_or_default(), by: handle.to_string() }),
        // `ask` and `say` fall back to the SPOOL — see spooling_once. Without it the one
        // channel a session has for reaching its human failed outright from inside the
        // sandbox every session runs in.
        "ask" => spooling_once(&send, &Request::OperatorAsk {
            session: handle.to_string(),
            question: body(arg),
            options: opt("--options").cloned(),
        }, &serde_json::json!({
            "op": "operator_ask", "session": handle,
            "question": body(arg), "options": opt("--options").cloned(),
        }), "queued — paosd raises it within ~5s. The escalation id is NOT available yet, \
             so use `paos operator listen` for the answer rather than `wait <id>`."),
        "answer" => match arg.and_then(|a| a.parse::<i64>().ok()) {
            None => { eprintln!("answer needs an escalation id"); 2 }
            Some(id) => once(&send, &Request::OperatorAnswer {
                id, text: body(positional.get(3)) }),
        },
        "resolve" => match arg.and_then(|a| a.parse::<i64>().ok()) {
            None => { eprintln!("resolve needs an escalation id"); 2 }
            Some(id) => once(&send, &Request::OperatorResolve { id }),
        },
        "park" => once(&send, &Request::OperatorPark {
            session: handle.to_string(), note: body(arg) }),
        "resolve-park" => match arg.and_then(|a| a.parse::<i64>().ok()) {
            None => { eprintln!("resolve-park needs a park id"); 2 }
            Some(id) => once(&send, &Request::OperatorResolvePark { id }),
        },
        "say" => spooling_once(&send, &Request::OperatorSay {
            session: handle.to_string(), text: body(arg) },
            &serde_json::json!({ "op": "operator_say", "session": handle, "text": body(arg) }),
            "queued — paosd delivers it within ~5s"),
        "send" => once(&send, &Request::OperatorSend { text: body(arg) }),
        other => {
            eprintln!("unknown operator subcommand: {other}");
            2
        }
    }
}

/// Like `once`, but degrades to the SPOOL instead of failing when the daemon is
/// unreachable.
///
/// The original refused to spool on purpose: `ask` returns an escalation id the caller
/// waits on, and reporting "queued" would hand back an id that does not exist yet. That
/// reasoning is sound and the CONSEQUENCE was not — every agent session runs in a sandbox
/// that blocks the unix socket, so the effect was that no session could reach its human
/// AT ALL. Measured 2026-08-01: 26 of 1,031 sessions ever did.
///
/// So spool, and be honest about what the caller does not get: the id. `say` never needed
/// one, and `ask` has `paos operator listen`, which does not take an id.
fn spooling_once(
    send: &impl Fn(&Request) -> Option<Response>,
    req: &Request,
    spooled: &serde_json::Value,
    queued_msg: &str,
) -> i32 {
    match send(req) {
        Some(Response::Ok { lines }) => { for l in lines { println!("{l}"); } 0 }
        Some(Response::Err { message, exit_code }) => { eprintln!("{message}"); exit_code }
        None => match crate::spool(spooled) {
            Some(_) => { println!("{queued_msg}"); 0 }
            None => {
                eprintln!("paos: paosd unreachable AND the spool is unwritable — \
                           NOTHING was sent to the operator.");
                super::EXIT_NO_DAEMON
            }
        },
    }
}

fn once(send: &impl Fn(&Request) -> Option<Response>, req: &Request) -> i32 {
    match send(req) {
        Some(Response::Ok { lines }) => { for l in lines { println!("{l}"); } 0 }
        Some(Response::Err { message, exit_code }) => { eprintln!("{message}"); exit_code }
        // No spool fallback on purpose. These are conversational: `ask` returns an id the
        // caller immediately waits on, and `say` is only useful if it actually reaches a
        // phone. Reporting "queued" for something the daemon may apply in five seconds
        // would hand back an id that does not exist yet.
        None => {
            eprintln!("paos: cannot reach paosd — the operator channel needs the daemon.\n\
                       Run this outside the agent sandbox, or check `paosctl doctor`.");
            super::EXIT_NO_DAEMON
        }
    }
}

/// Block until an escalation is answered, then print the answer.
///
/// Polls the database read-only rather than holding a socket: this must work from inside
/// a sandbox, and it can legitimately wait hours for a human.
fn wait_escalation(id: Option<i64>) -> i32 {
    let Some(id) = id else { eprintln!("wait needs an escalation id"); return 2 };
    loop {
        let Some(c) = ro() else { eprintln!("paos.db unreadable"); return 1 };
        match op::escalation_state(&c, id) {
            None => { eprintln!("no escalation #{id}"); return 2 }
            Some((status, answer)) => match status.as_str() {
                "answered" => {
                    println!("{}", answer.unwrap_or_default());
                    return 0;
                }
                // Resolved without an answer is a real outcome, not an error: the work
                // moved past the question. Exit non-zero so a script can tell them apart.
                "resolved" => { eprintln!("escalation #{id} was resolved without an answer"); return 3 }
                _ => {}
            },
        }
        std::thread::sleep(std::time::Duration::from_secs(POLL_SECS));
    }
}

/// Block until the operator says something, then print it and exit.
fn listen() -> i32 {
    loop {
        // Taking a message CONSUMES it, so this one read needs write access. It is the
        // one exception, and it is a single-row UPDATE on a queue the daemon only appends
        // to — not the kind of contention the single-writer rule exists for.
        let Ok(c) = rusqlite::Connection::open(paos_store::db_path()) else {
            eprintln!("paos.db unreadable");
            return 1;
        };
        let _ = c.busy_timeout(std::time::Duration::from_secs(5));
        match op::take_operator_message(&c) {
            Ok(Some(text)) => { println!("{text}"); return 0 }
            Ok(None) => {}
            Err(e) => { eprintln!("listen failed: {e}"); return 1 }
        }
        drop(c);
        std::thread::sleep(std::time::Duration::from_secs(POLL_SECS));
    }
}
