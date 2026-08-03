//! Bus reads for the degraded (socket-blocked) path.
//!
//! `who`, `whoami` and `reachable` are the three verbs a session runs to find out whether
//! it is still connected to the fleet — and until now all three were socket-only, so all
//! three failed with exit 69 inside the agent sandbox that is their only real caller.
//! Measured on 2026-07-31; the Python equivalents exit 0 because they read SQLite.
//!
//! Reads open the database read-only. Writes — `join`, `leave`, `send`, `status`, and the
//! room restore `reachable` performs — go to the SPOOL, which the daemon drains within
//! ~5s. The CLI never writes SQLite itself; that is the single-writer invariant paosd
//! exists to hold.

use paos_proto::Response;

/// "Listening" means **a live flock on `~/.paos/listen/<handle>.lock`** — the same fact
/// the Python listener establishes, and the only one visible from a sandbox.
///
/// The daemon answers this from `push.has_listener`, i.e. an open `Listen` socket. That
/// is a DIFFERENT fact: every session on this machine today arms `paos bus wait-joined`,
/// which takes the flock and holds no socket. Answering `reachable` from the push
/// registry would therefore tell every correctly-armed session it is NOT LISTENING, and
/// the documented response to that is to arm another listener.
fn listening(name: &str) -> bool {
    paos_bus::readonly::is_listening(&paos_store::root(), name)
}

/// `who` — the live roster.
pub fn who(archive: bool) -> Option<Response> {
    who_at(&paos_store::db_path(), &paos_store::root(), archive, now_epoch())
}

/// Split from `who` so a test can supply a database, a root and a clock instead of
/// mutating PAOS_ROOT, which is process-global and races the rest of the binary.
pub fn who_at(db: &std::path::Path, root: &std::path::Path, archive: bool, now: i64)
    -> Option<Response>
{
    let conn = paos_bus::readonly::open_ro(db)?;
    let rows = paos_bus::readonly::roster(&conn, archive).ok()?;
    if rows.is_empty() {
        return Some(Response::ok(
            if archive { "(no archived sessions)" } else { "(no sessions)" }));
    }
    Some(Response::Ok {
        lines: rows
            .iter()
            .map(|r| {
                let dnd = paos_bus::readonly::dnd_active(root, &r.name);
                paos_bus::readonly::render_roster_row(r, now, dnd)
            })
            .collect(),
    })
}

/// Seconds since the epoch. The roster's ages are relative to this.
fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `whoami` — which handle this Claude Code session is bound to.
pub fn whoami(session_id: Option<&str>) -> Option<Response> {
    // Identity comes from CLAUDE_CODE_SESSION_ID (the similarly-named CLAUDE_SESSION_ID is
    // a different variable that nothing sets), else the pointer file the hook writes —
    // both via the single shared resolver.
    let sid = session_id.map(str::to_string).or_else(crate::operator::session_id);
    whoami_at(sid.as_deref(), &paos_store::db_path())
}

/// Split from `whoami` so tests need not mutate CLAUDE_CODE_SESSION_ID or PAOS_ROOT.
/// Those are process-global and tests run in parallel — the exact arrangement that made
/// the doctor's spool check flaky.
fn whoami_at(sid: Option<&str>, db: &std::path::Path) -> Option<Response> {
    let sid = match sid {
        Some(s) if !s.trim().is_empty() => s.trim(),
        // Guessing here would bind this session to some other session's handle, and every
        // message addressed to it would go to a session that cannot read it.
        _ => return Some(Response::err(
            "no session id: pass --session-id or set CLAUDE_CODE_SESSION_ID", 2)),
    };
    let conn = paos_bus::readonly::open_ro(db)?;
    match paos_bus::readonly::whoami(&conn, sid) {
        Some(name) => Some(Response::ok(name)),
        None => Some(Response::err(format!("no live session bound to {sid}"), 3)),
    }
}

/// `reachable` — the self-heal probe.
///
/// It really does heal: rooms this handle was dropped from are re-joined by SPOOLING a
/// `bus_join`, which the daemon applies within ~5s. The CLI still never writes SQLite.
///
/// Reporting without repairing was not good enough. Every session runs in the sandbox, so
/// "diagnoses but does not heal" means the self-heal never fires for anyone — and a
/// session dropped from lobby stops receiving operator broadcasts while looking healthy.
pub fn reachable(name: &str) -> Option<Response> {
    reachable_spooling(&paos_store::db_path(), name,
                       &|p| { let _ = crate::spool(p); })
}

/// The probe with the repair channel stubbed out — a TEST helper, and marked as one.
///
/// Its doc comment used to say "kept for callers that must not cause a write". There are
/// no such callers and never were, which made it dead code in the binary wearing a
/// justification. `cargo check --tests` counted the test references and reported zero
/// warnings; `cargo build` compiles only the bin and reported it. Whichever target you
/// happened to compile decided the answer.
#[cfg(test)]
pub fn reachable_at(db: &std::path::Path, name: &str) -> Option<Response> {
    reachable_spooling(db, name, &|_| {})
}

/// The probe, with the repair channel injected.
///
/// `db` and `emit` are parameters rather than globals so a test needs neither PAOS_ROOT
/// nor a real spool directory. Setting PAOS_ROOT in a test races every other test in the
/// process — Rust runs them as threads sharing one environment — and the suite can then
/// write the live fleet store. The tree already takes a path for this reason in
/// `spool_at`, `drain_spool_at` and `read_only_recall_at`.
pub fn reachable_spooling(
    db: &std::path::Path,
    name: &str,
    emit: &dyn Fn(&serde_json::Value),
) -> Option<Response> {
    let conn = paos_bus::readonly::open_ro(db)?;
    let joined = paos_bus::joined_rooms(&conn, name).unwrap_or_default();
    let prior = paos_presence::prior_rooms(&conn, name).unwrap_or_default();
    let missing: Vec<String> = prior.iter().filter(|r| !joined.contains(r)).cloned().collect();

    // Report the room set AFTER the repair, as the Python does — it writes first and then
    // re-reads. Printing the pre-repair set would tell a session it is in fewer rooms than
    // it is about to be in, which is the opposite of what this verb is for.
    let mut restored = joined.clone();
    restored.extend(missing.iter().cloned());

    let mut lines = vec![format!(
        "rooms: {}",
        if restored.is_empty() { "(none)".to_string() } else { restored.join(", ") }
    )];
    if !missing.is_empty() {
        for room in &missing {
            emit(&serde_json::json!({ "op": "bus_join", "room": room, "name": name }));
        }
        lines.push(format!(
            "  restored {} dropped room(s): {}",
            missing.len(),
            missing.join(", ")
        ));
    }

    // TELL THE SESSION HOW TO REACH THE HUMAN, HERE, BECAUSE NOTHING ELSE DOES.
    //
    // Measured on 2026-08-01: of 1,031 sessions ever created, 26 have EVER reached the
    // operator by any means — 2.5%. Zero used `paos bus blocked`, the mechanism the global
    // CLAUDE.md names. That is not sessions ignoring an instruction; it is an instruction
    // with no trigger.
    //
    // A session has no passive way to learn the operator is away. The mode-change banner
    // is deliberately AMBIENT so it does not wake the fleet, which means a session mid-turn
    // never sees it; `who` does not carry it; and the digest is written at session start,
    // before the operator leaves. So the only way to find out is to run `paos operator
    // mode` unprompted — and nothing prompts it.
    //
    // `reachable` is the end-of-turn reflex EVERY session already runs, which makes it the
    // one place a line is guaranteed to be read. Shown only when the mode is not attended:
    // a notice that prints every turn becomes furniture, and this one must not.
    if let Some(m) = away_notice() {
        lines.push(m);
    }
    if listening(name) {
        lines.push("REACHABLE: rooms ok, listener live.".into());
        Some(Response::Ok { lines })
    } else {
        lines.push("NOT LISTENING — arm one now (Bash run_in_background, no trailing '&'):".into());
        lines.push("    paos bus wait-joined".into());
        Some(Response::Err { message: lines.join("\n"), exit_code: 1 })
    }
}

/// A line telling the session the operator is not at the terminal, and what to do.
///
/// `None` when the operator is attended — the normal case, where the session should just
/// ask in its own terminal and nothing needs saying.
pub fn away_notice() -> Option<String> {
    let conn = paos_bus::readonly::open_ro(&paos_store::db_path())?;
    away_line(paos_operator::get_mode(&conn))
}

/// The notice for a given mode — pure, so both the wording and the SILENCE can be tested
/// without a database.
pub fn away_line(mode: paos_operator::Mode) -> Option<String> {
    match mode {
        // Attended is the normal case and needs no line: the human is at the terminal.
        paos_operator::Mode::Attended => None,
        paos_operator::Mode::Away => Some(
            "OPERATOR IS AWAY — asking in your terminal reaches nobody, and `paos bus \
             blocked` only marks you blocked for peers.\n  \
             a question you need answered:  paos operator ask \"<question>\"\n  \
             something they should know:    paos operator say \"<update>\"".into()),
        // AUTONOMOUS IS NOT AWAY, and telling it to ask would contradict the mode.
        //
        // My first version printed the same "go ask" text for both, which was wrong twice
        // over: autonomous means "proceed on best judgment within policy and log the
        // decision", so asking defeats the point — and `may_push` only opens Telegram for
        // Away or a recent operator message, so an `ask` here would queue and notify
        // nobody. SKILL.md:399 already drew this distinction and my notice contradicted it.
        paos_operator::Mode::Autonomous => Some(
            "OPERATOR IS AUTONOMOUS — proceed on your own judgment within policy; do NOT \
             block waiting for an answer.\n  \
             worth them knowing later:      paos operator say \"<update>\"".into()),
    }
}

/// The individual peers a `--to` addresses, or empty for a broadcast.
///
/// `@all` yields nothing: a broadcast has no specific recipient to be deaf, and warning
/// about the whole fleet on every broadcast is how a useful warning becomes wallpaper.
///
/// `@operator` yields nothing either, and that is the more important exclusion. The
/// operator is a HUMAN ON TELEGRAM, not a session — they have no listener and never will,
/// so the check is not merely noisy, it is WRONG IN THE DANGEROUS DIRECTION. It told the
/// sender "will not wake them" about the one recipient the bridge reliably DOES wake, on
/// a phone, and then recommended `paos bus wake @operator`, which wakes sessions and does
/// nothing for a human. I hit this myself sending the operator a status reply.
///
/// Getting this wrong discourages exactly the behaviour the operator channel exists for,
/// which is the same failure as `operator say` being unreachable from a sandbox — a
/// session that tries to reach its human is told the attempt was futile.
pub fn addressable_peers(target: &str) -> Vec<String> {
    let all = paos_bus::ALL.trim_start_matches('@');
    let operator = paos_bus::OPERATOR.trim_start_matches('@');
    target
        .trim()
        .trim_start_matches('@')
        .split("__")
        .map(|p| p.trim().trim_start_matches('@').to_string())
        .filter(|p| !p.is_empty() && p != all && !p.eq_ignore_ascii_case(operator))
        .collect()
}

/// Warn when a two-party conversation is being held in the DIRECTORY room.
///
/// `lobby` is where every session joins to FIND peers — 16 members at the time of
/// writing. It is not a chat room, and the rule table in SKILL.md said so only by
/// implication: it defined each kind's LIFETIME and never its use. So this was not
/// sessions ignoring a rule, it was a rule that existed nowhere enforceable.
///
/// Measured before writing this: of recent lobby traffic, 219 messages were directed at
/// one peer against 74 genuine broadcasts. Three quarters of the directory was two
/// sessions talking past everyone else.
///
/// A NUDGE, not a refusal, and deliberately so. Refusing would break a legitimate case —
/// the first "@you and I should take this to a room" is itself directed and has to go
/// somewhere — and a bus that rejects messages is worse than one that is untidy. It goes
/// to STDERR so it never contaminates output a caller is parsing.
pub fn directory_chat_nudge(room: &str, target: &str) -> Option<String> {
    if room != "lobby" || target.trim() == paos_bus::ALL {
        return None;
    }
    if is_operator_target(target) {
        // The operator in lobby is a DIFFERENT mistake with a different fix, so it gets
        // its own text. "Go find a room to talk in" is unhelpful when the recipient is
        // the human — see operator_room_nudge.
        return operator_room_nudge(room, target);
    }
    Some(format!(
        "note: lobby is the DIRECTORY — every session is in it. A message aimed at \
         {target} belongs in a room:\n    paos bus join <room> --kind task --repos <repos>\n\
         Use lobby to FIND a peer and agree where to talk, not to hold the conversation."
    ))
}

/// The `(kind, repos)` a join should tag its room with, or `None` to tag nothing.
///
/// Extracted from the verb so it can be tested without touching the spool. Writing to the
/// real spool root from a test means mutating `PAOS_ROOT`, which is process-global and has
/// already flaked one suite here.
fn join_tagging(
    kind_flag: Option<&str>,
    repos: &str,
    existing_kind: Option<&str>,
) -> Option<(String, String)> {
    if kind_flag.is_none() && repos.trim().is_empty() {
        return None;   // a plain join must not rewrite a room's tags
    }
    // `--repos` alone keeps the kind the room already has; only a missing row falls back to
    // the default. Retagging a `fleet` room as `task` because someone declared its repos
    // would cut its lifetime from 14 days to 2, silently.
    let kind = kind_flag
        .or(existing_kind)
        .unwrap_or(paos_bus::readonly::DEFAULT_ROOM_KIND)
        .to_string();
    Some((kind, repos.trim().to_string()))
}

/// Nudge a session that just created a room without saying what it is about.
///
/// A room with no `--repos` gets a Telegram topic titled `# <room>` and nothing else, so
/// the operator reading the topic list on a phone cannot tell whose it is. That list is how
/// they choose what to open, and it is half the reason the group reads as a mess.
///
/// `lobby` is exempt: it is the directory, it belongs to no project, and `# lobby` is the
/// correct title for it.
fn untagged_room_nudge(room: &str, repos: &str) -> Option<String> {
    if room == "lobby" || !repos.trim().is_empty() {
        return None;
    }
    Some(format!(
        "note: '{room}' declares no repos, so its Telegram topic will read '# {room}' and \
         will not say which project it belongs to.\n  \
         paos bus join {room} --kind <directory|fleet|program|task> --repos <repo,repo>\n  \
         The kind also sets how long the room lives before it auto-closes."
    ))
}

/// Does `--to` name the human? Mirrors `targets_operator` in the daemon's bridge.
fn is_operator_target(target: &str) -> bool {
    let op = paos_bus::OPERATOR.trim_start_matches('@');
    target
        .split("__")
        .any(|p| p.trim().trim_start_matches('@').eq_ignore_ascii_case(op))
}

/// Warn when the operator is addressed FROM THE DIRECTORY ROOM.
///
/// The operator reported this directly: "Messages are now being routed to wrong rooms. For
/// example the graph discussion must happen in the agentic brain related room, not here."
///
/// The bridge is not misrouting. It routes each room to its own Telegram topic, faithfully
/// — so a message sent from `lobby` lands in General, no matter what it is about. Five of
/// the last three hours' operator-directed messages were sent from lobby, including
/// agentic-brain project reports, and they piled into General exactly as instructed.
///
/// THE ROOM IS THE ROUTING. That is the fact sessions do not have: choosing a room is not
/// a tidiness preference on the bus side, it picks which topic the human reads it in. A
/// session that sends from lobby because "that is where everyone is" has, without knowing
/// it, chosen General.
fn operator_room_nudge(room: &str, target: &str) -> Option<String> {
    Some(format!(
        "note: you are messaging {target} from '{room}', THE DIRECTORY ROOM — and the room \
         decides the Telegram TOPIC they read it in. From lobby it lands in General, \
         mixed with every other project's traffic.\n  \
         Send it from your project's room instead, so it arrives in that topic:\n    \
         paos bus send <your-room> --to @operator \"…\"\n  \
         Reported by the operator: project discussion arriving in General instead of the \
         topic for its project."
    ))
}

/// Catch "@operator" written in the BODY while `--to` is a broadcast.
///
/// This one is silent and therefore worse than untidiness. `targets_operator` — correctly —
/// does not treat `@all` as operator-addressed, because a broadcast aimed at the fleet must
/// not go to a phone. So a session that opens its message with "@operator STATUS ..." and
/// leaves `--to` at its default has written a status report THAT REACHES NO HUMAN, and gets
/// back a cheerful `sent -> @all`.
///
/// Measured over six hours: 6 messages did exactly this, from 3 different sessions, several
/// of them status reports the operator had explicitly asked for. Nobody was ignoring
/// anything — the mention looks like addressing, and on every other chat system it is.
fn mention_without_target_nudge(target: &str, text: &str) -> Option<String> {
    let op = paos_bus::OPERATOR.trim_start_matches('@');
    if is_operator_target(target) {
        return None;
    }
    // NARROWED ONCE, BY ITS OWN FALSE POSITIVES. The first version fired on any `@operator`
    // anywhere in the body, and the first two messages I sent after shipping it — both of
    // them telling PEERS about this very rule — tripped it. That is the wallpaper failure
    // documented above, arriving within minutes of the code that warns about it.
    //
    // What actually separates the cases is ADDRESSING vs DISCUSSING, and in real traffic
    // addressing is POSITIONAL: all 6 misses opened with the mention ("@operator STATUS …",
    // "@operator UPDATE …"). Discussion carries it mid-sentence ("if it is for the human,
    // use --to @operator").
    //
    // Checked against 8 hours of real traffic BEFORE changing it: 7 fire — every genuine
    // miss — and 4 are suppressed, every discussion case including my own two. A measured
    // boundary, not an intuition about what reads naturally.
    let mentions = text.split('\n').map(str::trim_start).any(|line| {
        let Some(rest) = line.strip_prefix('@') else { return false };
        // `get` rather than a slice: `op` is ASCII but `rest` need not be, and indexing into
        // the middle of a multi-byte char would panic on somebody's message.
        rest.get(..op.len()).is_some_and(|h| h.eq_ignore_ascii_case(op))
            // "@operator_notes" is a different word, not an address.
            && rest[op.len()..]
                .chars()
                .next()
                .is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
    });
    if !mentions {
        return None;
    }
    Some(format!(
        "WARNING: your message says '@{op}' but --to is {target}. IT WILL NOT REACH THEM.\n  \
         Only the `--to` field is routed to Telegram; a mention in the body is just text, \
         because a broadcast to the fleet must never land on a phone.\n  \
         If it is for the human:  paos bus send <room> --to @{op} \"…\"\n  \
         If it is for the fleet:  drop the @{op} from the text."
    ))
}

/// `paos bus <verb>` — the nested form, matching the Python 1:1.
///
/// The flat verbs (`paos who`, `paos send`, …) stay as PERMANENT aliases onto this same
/// code: the Rust paos-init hook calls `paosctl recall` flat, and a flat verb disappearing
/// would leave every session starting with no memory digest and nothing erroring.
///
/// Verbs still living in the Python say so and exit 2, rather than half-working. Silently
/// accepting a write that goes nowhere is the failure mode this port exists to remove.
/// Flags that CONSUME the next argument. Without this list the value lands in the
/// positionals: `bus session-start --session-id X --ppid 4242` put "4242" in positional
/// slot 2, which is the handle slot, so the session was very nearly created with the
/// handle "4242" instead of a minted one. Caught by an end-to-end diff against the
/// Python, not by any unit test — the tests all passed.
const VALUED_FLAGS: [&str; 10] = [
    "--session-id", "--ppid", "--to", "--file", "--tail", "--older-than", "--repos",
    "--set", "--identity", "--task",
];

/// Positionals for the bus, derived from raw argv rather than from the top-level parser.
///
/// The top-level parser cannot know which FACET flags take a value, so it is not able to
/// do this correctly; keeping the knowledge next to the verbs that use it is what stops
/// the two from drifting.
fn bus_positionals(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if VALUED_FLAGS.contains(&a) {
            i += 2; // the flag and its value
            continue;
        }
        if a.starts_with("--") {
            i += 1; // a boolean flag
            continue;
        }
        out.push(args[i].clone());
        i += 1;
    }
    out
}

pub fn run(all_args: &[String], args: &[String]) -> i32 {
    let positional = bus_positionals(all_args);
    let positional = &positional[..];
    let sub = positional.get(1).map(String::as_str).unwrap_or("who");
    let arg = positional.get(2).map(String::as_str);
    let flag = |n: &str| args.iter().any(|a| a == n);
    let opt = |n: &str| args.iter().position(|a| a == n).and_then(|i| args.get(i + 1));
    let tail = |d: usize| opt("--tail").and_then(|v| v.parse::<usize>().ok()).unwrap_or(d);
    let me = crate::operator::handle();

    let root = paos_store::root();
    let now = now_epoch();
    // Opened LAZILY, and only by the reads. A write must not require a readable database:
    // it goes to the spool, and the daemon applies it later. Opening up front meant
    // `send` and `join` failed outright whenever paos.db was missing or unreadable —
    // precisely the moment losing the message matters most.
    let ro = || -> Option<rusqlite::Connection> {
        match paos_bus::readonly::open_ro(&paos_store::db_path()) {
            Some(c) => Some(c),
            None => {
                eprintln!("paos.db is unreadable at {}", paos_store::db_path().display());
                None
            }
        }
    };
    macro_rules! conn {
        () => { match ro() { Some(c) => c, None => return 1 } };
    }

    match sub {
        "who" => {
            match who_at(&paos_store::db_path(), &root, flag("--archive"), now) {
                Some(r) => print_response(r),
                None => 1,
            }
        }
        "whoami" => match whoami(opt("--session-id").map(String::as_str)) {
            Some(r) => print_response(r),
            None => 1,
        },
        // Foreground, instant, and authoritative: it probes the actual flock. The
        // end-of-turn reflex is `listening` -> arm only if it prints "none", so a wrong
        // answer here either leaves a session deaf or makes every session double-arm.
        "listening" => match paos_bus::readonly::listener_pid(
            &paos_bus::readonly::listen_lock_path(&root, &me)) {
            Some(pid) => { println!("live pid={pid}"); 0 }
            None => { println!("none"); 1 }
        },
        "joined" => {
            for r in paos_bus::joined_rooms(&conn!(), &me).unwrap_or_default() {
                println!("{r}");
            }
            0
        }
        "log" => {
            let Some(room) = arg else { eprintln!("log needs <room>"); return 2 };
            let msgs = paos_bus::readonly::messages(&conn!(), room).unwrap_or_default();
            let n = opt("--tail").and_then(|v| v.parse::<usize>().ok());
            let start = n.map(|n| msgs.len().saturating_sub(n)).unwrap_or(0);
            for m in &msgs[start..] {
                println!("{}", paos_bus::readonly::format_msg(room, m));
            }
            0
        }
        "members" => {
            let Some(room) = arg else { eprintln!("members needs <room>"); return 2 };
            let rows = paos_bus::readonly::members(&conn!(), room).unwrap_or_default();
            if rows.is_empty() {
                println!("(no members)");
                return 0;
            }
            for m in &rows {
                let dnd = paos_bus::readonly::dnd_active(&root, &m.name);
                println!("{}", paos_bus::readonly::render_member(m, now, dnd));
            }
            0
        }
        "seen" => {
            let Some(room) = arg else { eprintln!("seen needs <room>"); return 2 };
            let msgs = paos_bus::readonly::messages(&conn!(), room).unwrap_or_default();
            let cursors = paos_bus::readonly::room_cursors(&conn!(), room).unwrap_or_default();
            let names = paos_bus::readonly::room_member_names(&conn!(), room).unwrap_or_default();
            for l in paos_bus::readonly::render_seen(&msgs, &cursors, &names, tail(10)) {
                println!("{l}");
            }
            0
        }
        "rooms" => {
            let rows = paos_bus::readonly::rooms(&conn!(), flag("--all")).unwrap_or_default();
            for l in paos_bus::readonly::render_rooms(&rows) {
                println!("{l}");
            }
            0
        }
        "history" => {
            // `history [who]` — the handle is a POSITIONAL, defaulting to us. It used to
            // pass `me` unconditionally, so the argument was accepted and ignored:
            // `paos bus history witty-bison-2` printed MY task log under their name, and
            // returned the identical output for a handle that does not exist at all. A
            // wrong answer rather than an error, and the reader has no way to tell.
            // Found by diffing every bus verb against the Python before the cutover.
            let who = arg
                .map(|a| paos_bus::readonly::safe_name(a.trim_start_matches('@')))
                .unwrap_or_else(|| me.clone());
            let rows = paos_bus::readonly::history(&conn!(), &who).unwrap_or_default();
            if rows.is_empty() {
                println!("(no task history for {who})");
                return 0;
            }
            for l in rows {
                println!("{l}");
            }
            0
        }
        // `status` with no text is a READ. With text it is a write, handled below.
        "status" if arg.is_none() && !flag("--clear") => {
            let s = paos_bus::readonly::get_status(&conn!(), &me);
            // An empty status reads as "(idle)", not as a blank line: a peer scanning for
            // who is free needs those to be distinguishable from a failed read.
            println!("status: {}", if s.is_empty() { "(idle)" } else { &s });
            0
        }
        "reachable" => match reachable(&me) {
            Some(r) => print_reachable(r),
            None => 1,
        },
        // --- writes: socket first, spool when the sandbox blocks it ---------------------
        "join" => {
            let Some(room) = arg else { eprintln!("join needs <room>"); return 2 };
            // `--kind` AND `--repos` WERE ACCEPTED AND THROWN AWAY — and this is the root
            // cause of the room mess the operator reported on 2026-08-01.
            //
            // SKILL.md has always documented the tagged form:
            //     paos bus join <room> --kind task --repos a,b
            // but join only ever emitted `bus_join`. The flags parse (they are in
            // VALUED_FLAGS, so they do not even land in the positionals), are silently
            // discarded, and the command prints success. The verb that actually sets them
            // is `paos bus kind <room> --set <k> --repos <r>`, which the skill does not
            // mention in the room-creation example anyone copies.
            //
            // So every session that followed the documentation created an UNTAGGED room:
            // no kind, so it inherits the default 2-day lifetime whatever it really is, and
            // no repos, so its Telegram topic cannot name the project it belongs to. That
            // is `motion-qbo-e2e-followups` — no repos, and nobody did anything wrong.
            //
            // Honour them here rather than correcting the documentation to point at a
            // second command: one call is what sessions already type, and a room's kind
            // matters most at the moment it is created.
            let kind_flag = opt("--kind").cloned();
            let repos = opt("--repos").cloned().unwrap_or_default();
            if let Some(k) = kind_flag.as_deref() {
                if !paos_bus::readonly::ROOM_KINDS.iter().any(|(n, _)| *n == k) {
                    let names: Vec<&str> =
                        paos_bus::readonly::ROOM_KINDS.iter().map(|(n, _)| *n).collect();
                    eprintln!("unknown kind '{k}' — one of: {}", names.join(", "));
                    return 2;
                }
            }
            // `--repos` alone must not silently retag the room's kind, so carry the kind it
            // already has. `ro()`, not the `conn!` macro: the macro RETURNS 1 when the
            // database is unreadable, which would abort a join before it is even queued.
            // An unreadable db here only costs us the existing kind.
            let existing = ro()
                .and_then(|c| paos_bus::readonly::rooms(&c, true).ok())
                .and_then(|rows| rows.into_iter().find(|r| r.room == room).map(|r| r.kind));
            let rc = write_op(&serde_json::json!({ "op": "bus_join", "room": room, "name": me,
                                                  "repo": session_repo() }),
                              &format!("joined '{room}' as {me}"));
            if rc != 0 {
                return rc;
            }
            if let Some((kind, repos)) = join_tagging(kind_flag.as_deref(), &repos, existing.as_deref()) {
                return write_op(
                    &serde_json::json!({
                        "op": "bus_kind", "room": room, "kind": kind, "repos": repos }),
                    &format!("  {room}\tkind={kind}\trepos={}",
                             if repos.is_empty() { "-" } else { &repos }),
                );
            }
            if let Some(n) = untagged_room_nudge(room, &repos) {
                eprintln!("{n}");
            }
            rc
        }
        "leave" => {
            let Some(room) = arg else { eprintln!("leave needs <room>"); return 2 };
            write_op(&serde_json::json!({ "op": "bus_leave", "room": room, "name": me }),
                     &format!("left '{room}'"))
        }
        "send" => {
            let Some(room) = arg else { eprintln!("send needs <room>"); return 2 };
            // The body is a positional, `--file`, or `-` for stdin. A send that silently
            // posts an empty body is worse than one that refuses.
            let text = match body(positional.get(3).map(String::as_str), opt("--file")) {
                Ok(t) => t,
                Err(e) => { eprintln!("{e}"); return 2 }
            };
            let target = opt("--to").cloned().unwrap_or_else(|| paos_bus::ALL.to_string());
            if let Some(nudge) = directory_chat_nudge(room, &target) {
                eprintln!("{nudge}");
            }
            if let Some(nudge) = mention_without_target_nudge(&target, &text) {
                eprintln!("{nudge}");
            }
            // TELL THE SENDER WHEN THE TARGET CANNOT HEAR THEM.
            //
            // `sent -> @peer` reads as delivered. It is not: it means the row was written.
            // If the target has no listener armed, nothing will wake them and the message
            // waits until they happen to look — which, for a session mid-turn, can be many
            // minutes. The sender then blocks on a reply that was never going to come.
            //
            // That is not hypothetical and it is not rare: it is the normal state of any
            // session doing a long turn, because the wake loop EXITS on delivery and is
            // re-armed at end of turn. The deaf window is exactly as long as the recipient's
            // turn, and it is worst for orchestrators — the longest turns, and the sessions
            // peers most need to reach. Reported by the operator after having to ping an
            // orchestrator by hand, in two separate projects.
            //
            // stderr, and never fatal: the message IS queued and will be read.
            for who in addressable_peers(&target) {
                if !paos_bus::readonly::is_listening(&paos_store::root(), &who) {
                    eprintln!(
                        "note: @{who} has NO listener armed — the message is queued but will \
                         not wake them.\n  They are probably mid-turn; it will be read when \
                         they re-arm. Do not block on a reply.\n  `paos bus wake @{who} \
                         \"<why>\" --room {room}` if it cannot wait."
                    );
                }
            }
            write_op(
                &serde_json::json!({
                    "op": "bus_send", "room": room, "sender": me, "target": target,
                    "text": text, "urgent": flag("--urgent"),
                }),
                &format!("sent -> {target}"),
            )
        }
        "status" => {
            let text = if flag("--clear") { None } else { arg };
            write_op(
                &serde_json::json!({ "op": "bus_status", "name": me, "status": text }),
                &match text {
                    Some(t) => format!("status set ({me}): {t}"),
                    None => format!("status cleared ({me})"),
                },
            )
        }
        "blocked" => {
            let Some(q) = arg else { eprintln!("blocked needs <question>"); return 2 };
            // The marker is what `who` and the supervisor key off to show BLOCKED.
            let text = format!("{} {q}", paos_bus::readonly::BLOCKED_MARKER);
            let rc = write_op(
                &serde_json::json!({ "op": "bus_status", "name": me, "status": text }),
                &format!("blocked ({me}): {q}"));
            // The bus never reaches a human. Saying so here is the whole point of the verb —
            // but WHICH human-reaching command to use depends on the mode, and this used to
            // say "ask your OWN operator (in this terminal)" unconditionally.
            //
            // That is correct only when ATTENDED. In away or autonomous there is nobody in
            // the terminal, so the advice sent a blocked session to talk to an empty room
            // while `paos operator ask` sat one line away. The global CLAUDE.md has drawn
            // this distinction for a while; this verb contradicted it, which is the same
            // shape as the `excludedCommands` advice fixed earlier today — the doc was
            // right and the tool disagreed.
            println!("→ peers will NOT relay this; the bus never reaches a human.");
            match away_notice() {
                Some(l) => println!("  {l}"),
                // Attended: they really are in the terminal, so the old advice stands.
                None => println!("  ask your OWN operator, in this terminal."),
            }
            println!("  clear with 'paos bus status --clear'.");
            rc
        }
        "hello" => hello(&root, &me, opt("--task").map(String::as_str), flag("--force")),
        "dnd" => {
            // A marker file, so it is readable from a sandbox and survives a restart.
            let dir = root.join("dnd");
            let p = dir.join(paos_bus::readonly::fs_safe(&me));
            match arg {
                Some("on") => {
                    let _ = std::fs::create_dir_all(&dir);
                    match std::fs::write(&p, "") {
                        Ok(_) => { println!("dnd ON ({me}) — urgent messages still get through"); 0 }
                        Err(e) => { eprintln!("could not set dnd: {e}"); 1 }
                    }
                }
                Some("off") => { let _ = std::fs::remove_file(&p); println!("dnd off ({me})"); 0 }
                _ => {
                    println!("{}", if p.exists() { "on" } else { "off" });
                    0
                }
            }
        }
        // The dashboard's wake button calls this verbatim; the contract must not drift.
        "wake" => {
            let Some(target) = arg else { eprintln!("wake needs <name> [text]"); return 2 };
            let text = positional.get(3).cloned().unwrap_or_else(|| "(wake)".into());
            // URGENT by definition: a wake exists to cost the recipient a turn, and it is
            // the one thing that penetrates DND.
            write_op(
                &serde_json::json!({
                    "op": "bus_send", "room": "lobby", "sender": me,
                    "target": format!("@{}", target.trim_start_matches('@')),
                    "text": text, "urgent": true,
                }),
                &format!("woke @{}", target.trim_start_matches('@')),
            )
        }
        // --- the wake loop -------------------------------------------------------------
        // `wait-joined` re-discovers joined rooms every window, so a task room joined
        // mid-session is listened to without re-arming. `wait <rooms>` pins them.
        "wait" | "wait-joined" | "listen" | "listen-joined" => {
            let rooms = match sub {
                "wait" | "listen" => arg.map(|r| {
                    r.split(',').map(str::trim).filter(|x| !x.is_empty())
                        .map(str::to_string).collect::<Vec<_>>()
                }),
                _ => None,
            };
            // A single window for `listen*`; the always-on loop for `wait*`.
            run_listener(me.clone(), rooms, sub.starts_with("listen"), false)
        }
        // Deliberately NOT silently accepted. These still live in the Python; pretending
        // to succeed is how a session believes it sent a message nobody will receive, or
        // believes it is listening when it is not.
        // Supervisor sweeps run in the daemon: they are bulk writes, and the pid check
        // must happen where the process table is readable. A sandboxed CLI cannot judge
        // any liveness but its own — `ps` and `pgrep` are denied there, and their failure
        // reads as "no such process" on exit code alone.
        "reap" => crate::dispatch(&paos_proto::Request::BusReap),
        "prune" => crate::dispatch(&paos_proto::Request::BusPrune {
            older_than_min: opt("--older-than").and_then(|v| v.parse().ok()).unwrap_or(60),
        }),
        // `recv` delivers everything unread, INCLUDING the ambient traffic a listener
        // deliberately did not wake for. Waking and delivering are different questions,
        // and this is the "catch me up" side of that split.
        // `read` and `inbox` are the names sessions and SKILL.md actually use — `inbox`
        // most of all. They were in the Python parser and NOT in this match, so after the
        // cutover `paos bus inbox` answered "unknown bus verb" for every session on the
        // machine. Found by typing it, not by a sweep: a verb that is absent from the
        // dispatch is invisible to a parity diff that only exercises verbs it knows about.
        "recv" | "recv-joined" | "read" | "inbox" => {
            let c = conn!();
            let targets: Vec<String> = match (sub, arg) {
                // `read <room>` scopes like `recv <room>`; `inbox` is always all-joined,
                // which is why it takes no room argument.
                ("recv" | "read", Some(r)) => vec![r.to_string()],
                _ => paos_bus::joined_rooms(&c, &me).unwrap_or_default(),
            };
            let mut printed = 0usize;
            let mut had_msgs = false;
            for r in &targets {
                let msgs = paos_bus::readonly::messages(&c, r).unwrap_or_default();
                if msgs.is_empty() { continue; }
                had_msgs = true;
                let cursor = paos_bus::readonly::effective_cursor(&c, &root, r, &me);
                let hits = paos_bus::wait::unread_for(&msgs, cursor, &me, false);
                let (deliver, skipped) =
                    paos_bus::wait::cap_backlog(&hits, paos_bus::wait::BACKLOG_MAX_DELIVER);
                if skipped > 0 {
                    println!("({skipped} older message(s) in '{r}' not shown — read them with: paos bus log {r})");
                }
                for m in &deliver {
                    println!("{}", paos_bus::readonly::format_msg(r, m));
                    printed += 1;
                }
                if let Some(top) = msgs.iter().map(|m| m.seq).max() {
                    let _ = crate::spool(&serde_json::json!({
                        "op": "bus_cursor", "room": r, "member": me, "seq": top }));
                    paos_bus::readonly::record_pending_cursor(&root, r, &me, top);
                }
            }
            if printed == 0 && had_msgs {
                // STDERR, deliberately — it is a note about the read, not a message. On
                // stdout it would contaminate anything piping `recv`. An armed listener
                // already advanced the cursor and delivered these, so "nothing unread" is
                // the healthy answer; saying so stops a session concluding "no messages".
                //
                // There is no counterpart for a room with NO messages at all: silence is
                // the whole output, and adding a line there would diverge from the Python.
                eprintln!("(no unread addressed messages in {} — a listener may have already \
                           delivered them; use 'paos bus log <room>' to see the transcript)",
                          targets.join(", "));
            }
            0
        }
        "close" => {
            let Some(room) = arg else { eprintln!("close needs <room>"); return 2 };
            let c = conn!();
            if room_is_closed(&c, room) { println!("'{room}' already closed"); return 0; }
            // Only a member may close a room. `--force` is for the operator dashboard,
            // which is never a member, and for orphaned 0-member rooms nobody could
            // otherwise close.
            let members = paos_bus::readonly::room_member_names(&c, room).unwrap_or_default();
            if !flag("--force") && !members.contains(&me) {
                eprintln!("not a member of '{room}' — only a member can close it (use --force to override)");
                return 1;
            }
            write_op(&serde_json::json!({ "op": "bus_close", "room": room }),
                     &format!("closed '{room}' ({} members evicted; history kept)", members.len()))
        }
        "topic" => {
            let Some(room) = arg else { eprintln!("topic needs <room> <title>"); return 2 };
            let title = positional.get(3).cloned().unwrap_or_default();
            write_op(&serde_json::json!({ "op": "bus_topic", "room": room, "topic": title }),
                     &format!("topic set for '{room}'"))
        }
        "kind" => {
            let Some(room) = arg else { eprintln!("kind needs <room> [<kind>]"); return 2 };
            let c = conn!();
            // Report format is a contract: `<room>\tkind=<k>\trepos=<r|->`, with a literal
            // dash when there are no repos so the column is never empty.
            let show = |kind: &str, repos: &str| {
                println!("{room}\tkind={kind}\trepos={}",
                         if repos.is_empty() { "-" } else { repos });
            };
            // The flag is `--set`, not a positional and not `--kind`. Accepting a
            // positional here would silently succeed where the Python prints usage and
            // exits 2, so a typo would behave differently in the two implementations
            // during the cutover.
            let Some(kind) = opt("--set").map(String::as_str) else {
                // No kind given: report the current one rather than guessing. An unknown
                // room still answers with its INFERRED kind, as the Python does — the
                // question is "what lifetime would this room get", which has an answer
                // whether or not the row exists yet.
                let rows = paos_bus::readonly::rooms(&c, true).unwrap_or_default();
                match rows.iter().find(|r| r.room == room) {
                    // A row that exists but carries an unrecognised kind is INFERRED from
                    // the name (handled inside `rooms`).
                    Some(r) => show(&r.kind, &r.repos),
                    // A room with NO row at all reports the plain default, not an
                    // inference. Matching the Python exactly: `room_meta` returns
                    // DEFAULT_ROOM_KIND for a missing row and only infers for a present
                    // one. The two paths disagree for `lobby` and `*-fleet` names, which
                    // is a real inconsistency in the original — reported rather than
                    // silently "improved", since changing it would alter which rooms
                    // auto-close and that is not a porting decision.
                    None => show(paos_bus::readonly::DEFAULT_ROOM_KIND, ""),
                }
                return 0;
            };
            if !paos_bus::readonly::ROOM_KINDS.iter().any(|(k, _)| *k == kind) {
                let names: Vec<&str> = paos_bus::readonly::ROOM_KINDS.iter().map(|(k, _)| *k).collect();
                eprintln!("unknown kind '{kind}' — one of: {}", names.join(", "));
                return 2;
            }
            let repos = opt("--repos").cloned().unwrap_or_default();
            let rc = write_op(&serde_json::json!({
                        "op": "bus_kind", "room": room, "kind": kind, "repos": repos }),
                     &format!("{room}\tkind={kind}\trepos={}",
                              if repos.is_empty() { "-" } else { &repos }));
            rc
        }
        "delete-room" => {
            let Some(room) = arg else { eprintln!("delete-room needs <room>"); return 2 };
            let c = conn!();
            let msgs: i64 = c.query_row("SELECT COUNT(*) FROM messages WHERE room=?1",
                                        [room], |r| r.get(0)).unwrap_or(0);
            let mems: i64 = c.query_row("SELECT COUNT(*) FROM members WHERE room=?1",
                                        [room], |r| r.get(0)).unwrap_or(0);
            let exists: i64 = c.query_row("SELECT COUNT(*) FROM rooms WHERE room=?1",
                                          [room], |r| r.get(0)).unwrap_or(0);
            if exists == 0 && msgs == 0 && mems == 0 {
                println!("(no such room '{room}')");
                return 0;
            }
            // Irreversible — unlike `close`, the transcript goes too. Show the cost first.
            if !flag("--force") {
                println!("would delete room '{room}': {msgs} messages, {mems} members, cursors — re-run with --force");
                return 2;
            }
            write_op(&serde_json::json!({ "op": "bus_delete_room", "room": room }),
                     &format!("deleted room '{room}' (freed)"))
        }
        "forget" => {
            let targets: Vec<&String> = positional.iter().skip(2).collect();
            if targets.is_empty() { eprintln!("forget needs <name>..."); return 2 }
            for t in &targets {
                let _ = crate::spool(&serde_json::json!({ "op": "bus_forget", "name": t }));
            }
            println!("forgot {} session(s)", targets.len());
            println!("  (queued — paosd applies it within ~5s)");
            0
        }
        "prune-rooms" => crate::dispatch(&paos_proto::Request::BusPruneRooms),
        // Pure output: a ready-to-paste briefing. It deliberately writes nothing to the
        // room — inviting someone is not the same as speaking for them.
        "invite" => {
            let Some(room) = arg else { eprintln!("invite needs <room>"); return 2 };
            if let Some(id) = opt("--identity") {
                if id == room {
                    eprintln!("warning: identity '{id}' equals the room name — choose a distinct identity");
                }
            }
            println!("{}", invitation(room, opt("--task").map(String::as_str),
                                     opt("--repo").map(String::as_str)));
            0
        }
        // Corrects a MIS-DERIVED identity — e.g. a harness renamed the worktree after the
        // name was frozen at session start. Deliberately persistent, which is exactly what
        // `--as` is not. Not for impersonating another session.
        "rename" => {
            let Some(new) = arg.map(paos_bus::readonly::fs_safe) else {
                eprintln!("rename needs <new-name>"); return 2
            };
            if new.is_empty() || new == "anon" {
                eprintln!("refusing to set an empty/invalid identity");
                return 1;
            }
            if new == me { println!("identity already '{new}'"); return 0; }
            let p = root.join(".identity").join(crate::operator::fs_key());
            if let Err(e) = std::fs::create_dir_all(p.parent().unwrap_or(&root))
                .and_then(|_| std::fs::write(&p, &new)) {
                eprintln!("could not persist identity: {e}");
                return 1;
            }
            // Move lobby presence so `who` and @-addressing follow the new name.
            let _ = crate::spool(&serde_json::json!({
                "op": "bus_leave", "room": "lobby", "name": me }));
            let _ = crate::spool(&serde_json::json!({
                "op": "bus_join", "room": "lobby", "name": new }));
            println!("identity set to '{new}' (was '{me}') — re-arm any listener under the new name");
            0
        }
        // THE SESSION LIFECYCLE. hooks/session-presence calls all three on EVERY TURN in
        // EVERY session, so this is the one place where a mistake is fleet-wide and
        // instant. Routed through the same send-or-degrade path as the flat verbs, so the
        // nested and flat forms cannot drift.
        //
        // The hook discards stdout (`stdout=subprocess.DEVNULL`), so nothing depends on
        // session-start ECHOING the minted handle — what matters is the sid->handle
        // binding, which `whoami` resolves from the database afterwards. That is what
        // makes spooling these safe: a queued write cannot return a value, and here
        // nothing reads one.
        "session-start" => {
            let Some(sid) = opt("--session-id").cloned().or_else(crate::operator::session_id)
            else { eprintln!("session-start needs --session-id"); return 2 };
            crate::dispatch(&paos_proto::Request::SessionStart {
                session_id: sid,
                // The handle is MINTED by whoever applies the write. Passing the current
                // one would rebind an existing session to a name derived from the
                // environment, which is how a rename loop starts.
                name: arg.unwrap_or_default().to_string(),
                pid: opt("--ppid").and_then(|p| p.parse().ok()),
            })
        }
        "heartbeat" => {
            let Some(sid) = opt("--session-id").cloned().or_else(crate::operator::session_id)
            else { eprintln!("heartbeat needs --session-id"); return 2 };
            crate::dispatch(&paos_proto::Request::Heartbeat {
                session_id: sid,
                // Re-asserted every turn, exactly as the Python does. The hook passes it
                // on every Stop; dropping it would leave the reaper reading a stale pid.
                pid: opt("--ppid").and_then(|p| p.parse().ok()),
            })
        }
        "session-end" => {
            let Some(sid) = opt("--session-id").cloned().or_else(crate::operator::session_id)
            else { eprintln!("session-end needs --session-id"); return 2 };
            crate::dispatch(&paos_proto::Request::SessionEnd { session_id: sid })
        }
        // The protocol self-heal: tell a session when SKILL.md has moved past what it
        // last acknowledged, and exactly what changed. Without this a session keeps
        // following a protocol that no longer exists — silently, because following stale
        // instructions does not error.
        // Reports only. The DRIFT NOTICE belongs in `hello`, where the Python puts it
        // and where a session actually meets it — on its first turn. Showing it here as
        // well would let a session that merely asked the version silently acknowledge a
        // protocol change it never read.
        "version" => {
            let path = paos_bus::skill::skill_md();
            let (cur, _) = paos_bus::skill::read(&path);
            println!("paos v{} · bus facet · SKILL.md v{} ({})",
                     env!("CARGO_PKG_VERSION"), cur.unwrap_or_else(|| "?".into()),
                     path.display());
            0
        }
        other => {
            eprintln!("unknown bus verb: {other}");
            2
        }
    }
}

/// The invitation text.
///
/// Extracted so it can be ASSERTED. The Python had five tests on this string and I had
/// none — the text was verified byte-identical CLI-to-CLI once, which proves it was right
/// that day and guards nothing afterwards. The peer framing in particular is load-bearing:
/// a session that reads an invitation as a chain of command routes its human's questions
/// to the inviter's terminal instead of its own.
pub fn invitation(room: &str, task: Option<&str>, repo: Option<&str>) -> String {
    let mut connect =
        "Your identity is auto-assigned (persisted per session) — do NOT pass --as.".to_string();
    if let Some(r) = repo { connect.push_str(&format!(" · you own repo: {r}")); }
    if let Some(t) = task { connect.push_str(&format!(" · task: {t}")); }
    [
        format!("You are being invited to the paos bus → room \"{room}\"."),
        "Use the paos skill (~/.claude/skills/paos/paos bus): read its communication \
         model + always-on wake loop, then join.".to_string(),
        "You are joining as an EQUAL PEER, not a subordinate — coordinate over the bus, \
         do your own repo's work yourself, and if you need a human, ask in YOUR OWN \
         terminal (do not route it back through the inviter).".to_string(),
        connect,
    ].join("\n")
}

/// `hello` — the first thing every session on this machine runs.
///
/// Order is load-bearing. `--task` is recorded BEFORE the DND and already-announced early
/// returns, because it was previously parsed and then dropped on the floor while the
/// global policy told every session to pass it — so `who`, the dashboard Fleet panel and
/// the roster showed a fleet of sessions with no stated purpose, blank by construction.
/// A session that re-runs hello still has a task, and being silent is not being idle.
fn hello(root: &std::path::Path, me: &str, task: Option<&str>, force: bool) -> i32 {
    hello_at(root, &paos_store::db_path(), &|p| { let _ = crate::spool(p); }, me, task, force)
}

/// The git repository this session is working in, for the roster's `repo=` column.
///
/// NOTHING WROTE `members.repo` AFTER THE RUST PORT. The column exists, rows created by
/// the Python still carry it, and the only production INSERT in paos-presence writes
/// `(room, name, joined_ts, last_seen)` — so every session created since the cutover has
/// shown `repo=-` in `paos bus who`. That column is how a peer decides whether a session
/// is even in the right codebase to ask, and SKILL.md documents it as part of the roster
/// contract, so it is not decoration.
///
/// Resolved HERE rather than in the daemon because only the CLI runs in the session's
/// working directory; paosd's cwd is wherever launchd started it.
pub fn session_repo() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let out = std::process::Command::new("git")
        .args(["-C", cwd.to_str()?, "rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let top = String::from_utf8(out.stdout).ok()?.trim().to_string();
    // Not in a repo: fall back to the cwd, which still tells a peer where you are. An
    // empty string would be recorded as a repo and render as one.
    Some(if top.is_empty() { cwd.to_string_lossy().into_owned() } else { top })
}

/// Split so the test supplies a root, a database and a sink — never by setting PAOS_ROOT,
/// which is process-global and lets any concurrent test fall back to the live store.
fn hello_at(
    root: &std::path::Path,
    db: &std::path::Path,
    emit: &dyn Fn(&serde_json::Value),
    me: &str,
    task: Option<&str>,
    force: bool,
) -> i32 {
    // 1. Join the lobby (idempotent). `repo` rides along so the roster can show which
    //    codebase this session is in — see session_repo().
    let repo = session_repo();
    emit(&serde_json::json!({ "op": "bus_join", "room": "lobby", "name": me, "repo": repo }));

    // 2. Record the task FIRST — see above.
    if let Some(t) = task.map(str::trim).filter(|t| !t.is_empty()) {
        emit(&serde_json::json!({ "op": "bus_status", "name": me, "status": t }));
    }

    // 3. Restore rooms this handle was dropped from. A reap or session-end clears
    //    `members`, so a session that restarts came back lobby-only and was silently deaf
    //    in its working rooms while `who` still showed it live. Cursors are preserved, so
    //    anything sent while away is delivered rather than skipped.
    let mut restored: Vec<String> = Vec::new();
    if let Some(conn) = paos_bus::readonly::open_ro(db) {
        let joined = paos_bus::joined_rooms(&conn, me).unwrap_or_default();
        for room in paos_presence::prior_rooms(&conn, me).unwrap_or_default() {
            if room == "lobby" || joined.contains(&room) || room_is_closed(&conn, &room) {
                continue;
            }
            emit(&serde_json::json!({ "op": "bus_join", "room": room, "name": me, "repo": repo }));
            restored.push(room);
        }
    }
    if !restored.is_empty() {
        println!("restored {} room(s): {}", restored.len(), restored.join(", "));
    }

    // 4. Announce — silent under DND, once per session otherwise.
    if paos_bus::readonly::dnd_active(root, me) {
        println!("dnd active — joined lobby");
        return 0;
    }
    let marker = root.join(".hello").join(paos_bus::readonly::fs_safe(me));
    if marker.exists() && !force {
        println!("already announced this session — skipping (use --force to re-announce)");
        return 0;
    }
    let _ = std::fs::create_dir_all(marker.parent().unwrap_or(root));
    let _ = std::fs::write(&marker, "");
    // Ambient presence: NO lobby broadcast. Online-ness lives in the roster and the
    // session.hello event; broadcasting it woke every peer for nothing.
    println!("online as {me} · joined lobby · presence registered");

    // The protocol self-heal, and the reason it lives HERE: every session runs `hello` on
    // its first turn, so this is the one moment it is guaranteed to be read. A session
    // whose acknowledged version is behind SKILL.md is told what changed and to re-read
    // it — otherwise it keeps following a protocol that no longer exists, silently,
    // because following stale instructions does not error.
    let (cur, changelog) = paos_bus::skill::read(&paos_bus::skill::skill_md());
    if let Some(cur) = cur {
        let seen: Option<String> = paos_bus::readonly::open_ro(db)
            .and_then(|c| c.query_row(
                "SELECT ack_skill_version FROM sessions WHERE name = ?1",
                [me], |r| r.get::<_, Option<String>>(0)).ok())
            .flatten();
        if let Some(n) = paos_bus::skill::notice(seen.as_deref(), &cur, &changelog) {
            println!("{n}");
            // Recorded so it does not repeat every turn: a banner shown unheeded on every
            // turn is noise, and noise is how a real one stops being read.
            emit(&serde_json::json!({
                "op": "bus_ack_version", "name": me, "version": cur }));
        }
    }
    0
}

fn room_is_closed(conn: &rusqlite::Connection, room: &str) -> bool {
    conn.query_row("SELECT closed_ts FROM rooms WHERE room = ?1", [room],
                   |r| r.get::<_, Option<String>>(0))
        .ok().flatten().is_some()
}

/// The sweeps, when the socket is blocked.
///
/// Each COUNTS read-only what the sweep would touch, then queues the write. "queued"
/// alone was rejected — and rightly: an operator runs a sweep exactly when they suspect
/// something is wrong, and it cannot distinguish swept 0 from swept 400 from never ran.
/// The number here is real; only the moment the rows change is deferred.
fn preview<F>(op: &str, count: F, render: &dyn Fn(usize) -> String) -> Option<Response>
where
    F: Fn(&rusqlite::Connection, i64) -> usize,
{
    let conn = paos_bus::readonly::open_ro(&paos_store::db_path())?;
    let n = count(&conn, now_epoch());
    let _ = crate::spool(&serde_json::json!({ "op": op }));
    Some(Response::Ok {
        lines: vec![render(n), "  (queued — paosd applies it within ~5s)".to_string()],
    })
}

pub fn preview_reap() -> Option<Response> {
    preview("bus_reap",
        |c, now| {
            // Exactly the reaper's rule, read-only: a confirmed-alive pid protects,
            // otherwise the heartbeat backstop decides. Duplicating the predicate would
            // let the preview and the sweep disagree, so it asks paos-presence.
            paos_presence::reap_candidates(c, now).map(|v| v.len()).unwrap_or(0)
        },
        &|n| format!("would reap {n} dead session(s)"))
}

pub fn preview_prune(mins: i64) -> Option<Response> {
    let conn = paos_bus::readonly::open_ro(&paos_store::db_path())?;
    let stale = paos_presence::prune_candidates(&conn, now_epoch(), mins).unwrap_or_default();
    let _ = crate::spool(&serde_json::json!({
        "op": "bus_prune", "older_than_min": mins }));
    if stale.is_empty() {
        return Some(Response::ok("(nothing to prune)"));
    }
    let mut lines = vec![format!(
        "would prune {} stale member(s) (older than {mins}m):", stale.len())];
    lines.extend(stale.iter().map(|(room, name)| format!("  {name}@{room}")));
    lines.push("  (queued — paosd applies it within ~5s)".into());
    Some(Response::Ok { lines })
}

pub fn preview_prune_rooms() -> Option<Response> {
    let conn = paos_bus::readonly::open_ro(&paos_store::db_path())?;
    let (closed, purged) = paos_presence::room_gc_candidates(&conn, now_epoch())
        .unwrap_or((0, 0));
    let _ = crate::spool(&serde_json::json!({ "op": "bus_prune_rooms" }));
    Some(Response::Ok { lines: vec![
        format!("room GC: would close={closed} purge={purged}"),
        "  (queued — paosd applies it within ~5s)".into(),
    ]})
}

/// The listener, shared by `paos bus wait|listen` and the flat `paos listen`.
///
/// ONE implementation on purpose. The flat verb used to send `Request::Listen` down the
/// socket, so it exited 69 inside the sandbox that is its only real caller while the
/// nested form polled read-only and worked — two meanings of "listen", one of which
/// could not run where sessions live. That is the same shape as the two meanings of
/// "listening" that made `reachable` tell armed sessions they were deaf.
pub fn run_listener(name: String, rooms: Option<Vec<String>>, once: bool, urgent_only: bool)
    -> i32
{
    let env = crate::listen::Env {
        root: paos_store::root(),
        db: paos_store::db_path(),
        name,
        rooms,
        // The single biggest token lever: without this every idle session wakes on every
        // fleet broadcast. The env var restores the old behaviour.
        broadcast_wakes: matches!(
            std::env::var("PAOS_BUS_ALL_WAKES").unwrap_or_default().as_str(),
            "1" | "true" | "yes" | "on"),
        schedule: paos_bus::wait::WAIT_SCHEDULE.to_vec(),
        steady: paos_bus::wait::WAIT_STEADY,
        poll: std::time::Duration::from_secs(1),
        emit: &|p| { let _ = crate::spool(p); },
        out: &|s| println!("{s}"),
        max_windows: if once { Some(1) } else { None },
        urgent_only_override: urgent_only,
    };
    crate::listen::wait(&env, true)
}

/// `paos listen <name> <rooms>` — the flat alias. One window, like the nested `listen`.
pub fn listen_once(name: String, rooms: Option<Vec<String>>, urgent_only: bool) -> i32 {
    run_listener(name, rooms, true, urgent_only)
}

/// Apply a bus write: spool it, then confirm.
///
/// The CLI never writes SQLite — that is the invariant the daemon exists to hold — so the
/// write goes to the spool and the daemon applies it within ~5s. The confirmation says
/// which happened rather than a bare "ok": a session that believes a message was
/// delivered when it is still queued will draw the wrong conclusion from silence.
fn write_op(payload: &serde_json::Value, done: &str) -> i32 {
    match crate::spool(payload) {
        Some(_) => {
            println!("{done}");
            println!("  (queued — paosd applies it within ~5s)");
            0
        }
        None => {
            eprintln!("could not queue the write: the spool directory is not writable");
            1
        }
    }
}

/// A message body from a positional, `--file`, or stdin via `-`.
///
/// Refuses an empty body. `send` reporting success for a message with no content is the
/// same class of lie as `send` reporting success for a message addressed to nobody.
fn body(positional: Option<&str>, file: Option<&String>) -> Result<String, String> {
    use std::io::Read;
    let raw = match (positional, file) {
        (_, Some(f)) => std::fs::read_to_string(f)
            .map_err(|e| format!("cannot read --file {f}: {e}"))?,
        (Some("-"), None) => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s).map_err(|e| format!("cannot read stdin: {e}"))?;
            s
        }
        (Some(t), None) => t.to_string(),
        (None, None) => return Err("send needs <text>, --file <path>, or - for stdin".into()),
    };
    if raw.trim().is_empty() {
        return Err("refusing to send an empty message".into());
    }
    Ok(raw)
}

/// Print a `Response` the way the top-level dispatcher does, and return its exit code.
fn print_response(r: Response) -> i32 {
    match r {
        Response::Ok { lines } => {
            for l in lines {
                println!("{l}");
            }
            0
        }
        Response::Err { message, exit_code } => {
            eprintln!("{message}");
            exit_code
        }
    }
}

/// `reachable` prints its body to STDOUT even when it exits non-zero.
///
/// NOT LISTENING is a normal, expected state — it is the answer, not a failure to
/// produce one — and the Python prints all three lines to stdout with exit 1. Sending
/// them to stderr instead looks identical when the streams are merged, which is why the
/// earlier parity runs missed it: a session doing `out=$(paos bus reachable)` gets the
/// rooms and the repair advice from the Python and NOTHING from us, while the exit code
/// says the same thing either way. Caught by driving the real CLIs against each other
/// with the streams kept apart.
fn print_reachable(r: Response) -> i32 {
    match r {
        Response::Ok { lines } => { for l in lines { println!("{l}"); } 0 }
        Response::Err { message, exit_code } => { println!("{message}"); exit_code }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listening_is_decided_by_the_flock_not_the_push_registry() {
        // The two notions disagree for every session on this machine: `wait-joined` takes
        // the flock and opens no socket. If this ever starts consulting the push registry,
        // `reachable` tells correctly-armed sessions they are deaf and they double-arm.
        let base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
        let d = std::path::PathBuf::from(base).join(format!("paos-cli-bus-{}", std::process::id()));
        std::fs::create_dir_all(d.join("listen")).unwrap();
        assert!(!paos_bus::readonly::is_listening(&d, "nobody-home"));
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
        let d = std::path::PathBuf::from(base)
            .join(format!("paos-cli-bus-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_invitation_states_the_peer_frame_and_interpolates_its_variables() {
        // Ported from InviteTest, which had FIVE tests on this text and Rust had none.
        // "Verified byte-identical once" proves it was right that day; it guards nothing
        // afterwards, and this string is read by a session deciding how to treat the
        // sender.
        let s = invitation("myroom", Some("build the thing"), Some("widgets"));
        assert!(s.contains("room \"myroom\""), "{s}");
        assert!(s.contains("paos skill"), "{s}");
        assert!(s.contains("build the thing"), "the task must be interpolated: {s}");
        assert!(s.contains("you own repo: widgets"), "the repo must be interpolated: {s}");
        // THE LOAD-BEARING PART: being invited does not make the inviter your boss, and a
        // session that believes otherwise routes its human's questions to the wrong
        // terminal.
        assert!(s.contains("EQUAL PEER"), "{s}");
        assert!(s.to_lowercase().contains("your own terminal"), "{s}");
        assert!(s.contains("do NOT pass --as"), "{s}");
    }

    #[test]
    fn the_invitation_omits_the_optional_clauses_when_absent() {
        let s = invitation("myroom", None, None);
        assert!(s.contains("auto-assigned") && s.contains("do NOT pass --as"), "{s}");
        assert!(!s.contains("· task:"), "no task clause when none given: {s}");
        assert!(!s.contains("you own repo:"), "no repo clause when none given: {s}");
    }

    #[test]
    fn reachable_prints_its_body_to_stdout_even_when_it_exits_nonzero() {
        // NOT LISTENING is the ANSWER, not a failure to produce one, and the Python prints
        // all three lines to stdout with exit 1. We sent them to stderr, which is
        // indistinguishable when the streams are merged — so every earlier parity run
        // passed. A session doing `out=$(paos bus reachable)` got the rooms and the repair
        // advice from the Python and NOTHING from us, with the same exit code.
        //
        // Source-scanned because the behaviour is which STREAM a println goes to, which a
        // unit test cannot observe without capturing process-level fds. The risk being
        // guarded is someone "tidying" this back to print_response.
        let src = include_str!("bus.rs");
        let arm = src.split("\"reachable\" => match reachable(&me)").nth(1)
            .expect("the reachable dispatch arm exists");
        let head = &arm[..arm.find("},").unwrap_or(arm.len())];
        assert!(head.contains("print_reachable"),
                "the reachable arm must use print_reachable (stdout), not print_response \
                 (stderr) — the Python prints this body to stdout with exit 1");
    }

    #[test]
    fn hello_records_the_task_and_does_it_before_any_early_return() {
        // REGRESSION: `--task` was parsed and dropped, while the global policy tells every
        // session to pass it. `who`, the dashboard Fleet panel and the roster therefore
        // showed a whole fleet with no stated purpose — blank by construction.
        //
        // Recorded BEFORE the DND and already-announced early returns: a session that
        // re-runs hello still has a task, and being silent is not being idle. Both early
        // paths are exercised here, because that ordering is the actual fix.
        let d = tmp("hello");
        let db = d.join("paos.db");
        let _ = paos_store::open(&db).unwrap();
        let ops = std::cell::RefCell::new(Vec::new());
        let emit = |v: &serde_json::Value| ops.borrow_mut().push(v.clone());

        // Path 1: DND on — the announcement is suppressed, the task must still be recorded.
        std::fs::create_dir_all(d.join("dnd")).unwrap();
        std::fs::write(d.join("dnd").join("me"), "").unwrap();
        assert_eq!(hello_at(&d, &db, &emit, "me", Some("porting the bus"), false), 0);
        {
            let o = ops.borrow();
            let status = o.iter().find(|x| x["op"] == "bus_status")
                .expect("the task must be recorded even when DND suppresses the announcement");
            assert_eq!(status["status"], "porting the bus");
            assert!(o.iter().any(|x| x["op"] == "bus_join" && x["room"] == "lobby"));
        }

        // Path 2: already announced — the other early return, same requirement.
        std::fs::remove_file(d.join("dnd").join("me")).unwrap();
        std::fs::create_dir_all(d.join(".hello")).unwrap();
        std::fs::write(d.join(".hello").join("me"), "").unwrap();
        ops.borrow_mut().clear();
        assert_eq!(hello_at(&d, &db, &emit, "me", Some("second turn"), false), 0);
        let o = ops.borrow();
        assert_eq!(o.iter().find(|x| x["op"] == "bus_status").expect("task on re-run")["status"],
                   "second turn", "re-running hello must not lose the task");
    }

    #[test]
    fn whoami_without_any_session_id_fails_loudly_rather_than_guessing() {
        // Silently resolving to "some session" is how a message gets addressed to a handle
        // that cannot read it.
        let db = tmp("noid").join("paos.db");
        let _ = paos_store::open(&db).unwrap();
        for absent in [None, Some(""), Some("   ")] {
            match whoami_at(absent, &db) {
                Some(Response::Err { exit_code, .. }) => assert_eq!(exit_code, 2),
                other => panic!("expected a loud failure for {absent:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn whoami_resolves_a_live_handle_and_refuses_a_retired_one() {
        let db = tmp("resolve").join("paos.db");
        {
            let c = paos_store::open(&db).unwrap();
            paos_presence::session_start(&c, "sid-live", "live-otter", None, "t").unwrap();
            paos_presence::session_start(&c, "sid-gone", "gone-bison", None, "t").unwrap();
            paos_presence::session_end(&c, "sid-gone", "t").unwrap();
        }
        match whoami_at(Some("sid-live"), &db) {
            Some(Response::Ok { lines }) => assert_eq!(lines, vec!["live-otter".to_string()]),
            other => panic!("expected the bound handle, got {other:?}"),
        }
        // A retired handle must not resolve — exit 3, not a stale name.
        match whoami_at(Some("sid-gone"), &db) {
            Some(Response::Err { exit_code, .. }) => assert_eq!(exit_code, 3),
            other => panic!("expected exit 3 for an archived session, got {other:?}"),
        }
    }

    #[test]
    fn reachable_advises_the_command_sessions_actually_run() {
        // The daemon's copy prints `paos listen <handle> <rooms>`, which is not the verb
        // the wake loop documents. A repair hint nobody can follow is not a repair.
        let base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
        let d = std::path::PathBuf::from(base).join(format!("paos-hint-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let db = d.join("paos.db");
        { let _ = paos_store::open(&db).unwrap(); }
        // No PAOS_ROOT mutation: the env is process-global and Rust tests are threads.
        match reachable_at(&db, "some-handle") {
            Some(Response::Err { message, exit_code }) => {
                assert_eq!(exit_code, 1);
                assert!(message.contains("paos bus wait-joined"), "got: {message}");
            }
            other => panic!("expected NOT LISTENING, got {other:?}"),
        }
    }

    #[test]
    fn a_directed_message_in_the_directory_is_nudged_toward_a_room() {
        // Measured on the live store before this existed: 219 directed messages in lobby
        // against 74 real broadcasts. lobby has 16 members — a two-party conversation
        // there is held in front of the whole fleet.
        let n = directory_chat_nudge("lobby", "@witty-bison-2").expect("should nudge");
        assert!(n.contains("DIRECTORY"));
        assert!(n.contains("paos bus join"), "must say what to do instead, not just object");
    }

    #[test]
    fn a_broadcast_in_the_directory_is_exactly_what_lobby_is_for() {
        // Over-nudging would train sessions to ignore the nudge, which is how the
        // backtick warning failed: a rule that fires when it should not stops being read.
        assert!(directory_chat_nudge("lobby", "@all").is_none());
        assert!(directory_chat_nudge("lobby", " @all ").is_none());
    }

    #[test]
    fn a_directed_message_in_a_real_room_is_the_correct_behaviour() {
        assert!(directory_chat_nudge("ad-hocs", "@witty-bison-2").is_none());
        assert!(directory_chat_nudge("motion-fleet", "@quiet-otter").is_none());
    }


    #[test]
    fn every_verb_the_python_parser_offered_is_still_dispatched() {
        // `paos bus inbox` answered "unknown bus verb" for every session after the
        // cutover. It was in the Python parser and not in this match, and no parity sweep
        // caught it: a diff only exercises verbs it already knows about, so a verb ABSENT
        // from the dispatch is invisible to it. I found it by typing it.
        //
        // The list is the Python argparse surface, verbatim. Derived from the source below
        // rather than restated, so a verb dropped from the match fails here.
        const PYTHON_VERBS: [&str; 40] = [
            "send", "join", "recv", "read", "recv-joined", "inbox", "listen",
            "listen-joined", "wait", "wait-joined", "listening", "reachable", "kind",
            "joined", "log", "members", "seen", "leave", "forget", "rooms", "close",
            "delete-room", "topic", "invite", "dnd", "wake", "whoami", "rename", "status",
            "history", "who", "hello", "version", "session-start", "session-end",
            "heartbeat", "reap", "prune", "prune-rooms", "blocked",
        ];
        let src = include_str!("bus.rs");
        // Everything after the FIRST "match sub {". Two earlier attempts got this wrong
        // in the same direction and both reported 18 dispatched verbs as missing:
        // slicing to the first "\n    }" lands inside the first ARM, and split().nth(1)
        // returns only the text BETWEEN the two `match sub {` in this file. A scan that
        // reads less than it claims fails loudly here, which is the good case — the
        // dangerous version of this mistake passes.
        // From the first "match sub {" to the START OF THIS TEST MODULE. Bounding at both
        // ends is the whole point: the previous version ran to end-of-file, so it also
        // scanned PYTHON_VERBS itself and every verb "appeared" — the test could not fail.
        // Caught only by deleting a verb from the dispatch and watching it still pass.
        let after = src.split_once("match sub {").expect("the verb dispatch").1;
        let dispatch = &after[..after.find("#[cfg(test)]").unwrap_or(after.len())];
        let mut missing: Vec<&str> = Vec::new();
        for v in PYTHON_VERBS {
            if !dispatch.contains(&format!("\"{v}\"")) {
                missing.push(v);
            }
        }
        assert!(missing.is_empty(),
                "these verbs existed in the Python and are not dispatched here: {missing:?}");
    }


    #[test]
    fn a_broadcast_has_no_specific_peer_to_warn_about() {
        // Warning about every session on every @all would make the notice wallpaper, and a
        // warning people scroll past is worse than none.
        assert!(addressable_peers("@all").is_empty());
        assert!(addressable_peers(" @all ").is_empty());
        assert!(addressable_peers("").is_empty());
    }

    #[test]
    fn every_named_recipient_is_checked_including_multi_target() {
        assert_eq!(addressable_peers("@witty-bison-2"), vec!["witty-bison-2"]);
        assert_eq!(addressable_peers("@a__b"), vec!["a", "b"]);
    }

    #[test]
    fn addressing_the_operator_from_the_directory_names_the_topic_problem() {
        // The generic "take it to a room" text is WRONG here and that is the whole point of
        // the split: the fix is not "find a peer and agree where to talk", it is "the room
        // you send from is the topic they read it in".
        let n = directory_chat_nudge("lobby", "@operator").expect("must warn");
        assert!(n.contains("TOPIC") || n.contains("topic"), "must explain the routing: {n}");
        assert!(n.contains("General"), "must name where it actually lands: {n}");
        assert!(!n.contains("FIND a peer"), "the peer text does not apply to a human: {n}");
        // From a project room it is CORRECT — that is the behaviour being asked for, so
        // warning there would train sessions to ignore the warning that matters.
        assert!(directory_chat_nudge("agentic-brain-e2e", "@operator").is_none());
        // Case and the multi-target form must not slip past, since the bridge accepts both.
        assert!(directory_chat_nudge("lobby", "@Operator").is_some());
        assert!(directory_chat_nudge("lobby", "@operator__frosty-shrike").is_some());
    }

    #[test]
    fn a_body_mention_of_the_operator_with_a_broadcast_target_is_flagged() {
        // The silent one. `sent -> @all` after writing "@operator STATUS ..." reads as a
        // delivered report; nothing reached a phone. 6 real occurrences in 6 hours.
        // THE FIXTURES ARE REAL MESSAGES, copied from the bus rather than invented, because
        // the boundary this function draws was derived from them. An invented example would
        // let me re-derive the same wrong rule I shipped an hour ago.
        //
        // MUST FIRE — genuine misses. Each of these believed it had reported to the human.
        for real in [
            "@operator STATUS + THE ORCHESTRATOR ANSWER YOU ASKED FOR, measured just now",
            "@operator UPDATE, correcting my own message: THE WAKE DID NOT WORK.",
            "@operator — NOT MY AREA (I own BI/semantic layer; @cosmic-quokka-3 does)",
            // Addressed in a later paragraph rather than the opening line — still an address.
            "@cosmic-quokka-3 SPLIT ACCEPTED\n\n@operator for your call: which half first?",
        ] {
            let n = mention_without_target_nudge("@all", real)
                .unwrap_or_else(|| panic!("must warn on a real miss: {real}"));
            assert!(n.contains("WILL NOT REACH"), "the consequence must be explicit: {n}");
        }
        // MUST NOT FIRE — discussion, not addressing. The first two are MY OWN messages,
        // which is how this narrowing was found: I shipped the broad version and the very
        // next thing I sent tripped it.
        for discussion in [
            "FIX: if it is for the human, it needs --to @operator. Only --to is routed.",
            "- My own warning fired on @operator every time, which is wrong.",
            "continuing per the operator instruction",
            "see operator_outbox and operator.rs",
            "@operator_notes is a different word and must not match",
        ] {
            assert!(mention_without_target_nudge("@all", discussion).is_none(),
                    "discussing the convention must stay silent: {discussion}");
        }
        // Correctly addressed: silent regardless of body.
        assert!(mention_without_target_nudge("@operator", "@operator status").is_none());
        assert!(mention_without_target_nudge("@a__operator", "@operator status").is_none());
        // A peer target is the same silent failure as a broadcast — it reaches no phone.
        assert!(mention_without_target_nudge("@frosty-shrike", "@operator can you rule on this")
                .is_some());
        // Must not panic on a multi-byte body: `op` is ASCII, the message is not.
        assert!(mention_without_target_nudge("@all", "héllo — @operator?").is_none());
        assert!(mention_without_target_nudge("@all", "@opér").is_none());
    }

    #[test]
    fn join_applies_the_kind_and_repos_it_was_given() {
        // THE ROOT CAUSE OF THE ROOM MESS. SKILL.md has always documented
        // `paos bus join <room> --kind task --repos a,b`, and join emitted only `bus_join`:
        // both flags parsed, were discarded, and the command reported success. Every
        // session that followed the documentation created an untagged room — default
        // 2-day lifetime whatever it really was, and no repos, so its Telegram topic could
        // not name its project. `motion-qbo-e2e-followups` is one, and nobody erred.
        assert_eq!(join_tagging(Some("fleet"), "dotfiles", None),
                   Some(("fleet".into(), "dotfiles".into())));

        // A PLAIN JOIN MUST CHANGE NOTHING. Joining an existing room is the common case —
        // every session does it on every first turn — and re-tagging it from a bare join
        // would let the last joiner silently redefine a room's lifetime.
        assert_eq!(join_tagging(None, "", Some("fleet")), None);
        assert_eq!(join_tagging(None, "   ", Some("fleet")), None, "whitespace is not a repo");

        // `--repos` alone keeps the kind the room already has. Defaulting to `task` here
        // would cut a fleet room's lifetime from 14 days to 2 because somebody helpfully
        // declared its repos — a silent change to when it auto-closes.
        assert_eq!(join_tagging(None, "motion", Some("fleet")),
                   Some(("fleet".into(), "motion".into())));
        // ...and with no row at all there is nothing to preserve, so the default stands.
        assert_eq!(join_tagging(None, "motion", None),
                   Some((paos_bus::readonly::DEFAULT_ROOM_KIND.to_string(), "motion".into())));
        // An explicit kind always wins over the stored one — that is what re-tagging means.
        assert_eq!(join_tagging(Some("task"), "", Some("fleet")),
                   Some(("task".into(), String::new())));
    }

    #[test]
    fn a_room_created_without_repos_is_told_what_it_costs() {
        let n = untagged_room_nudge("motion-qbo-e2e-followups", "").expect("must nudge");
        assert!(n.contains("# motion-qbo-e2e-followups"),
                "must show the title the operator will actually see: {n}");
        // Declaring repos is the whole point — silent once it is done.
        assert!(untagged_room_nudge("motion-qbo-e2e-followups", "motion").is_none());
        // lobby is the directory and belongs to no project; `# lobby` is correct for it.
        assert!(untagged_room_nudge("lobby", "").is_none(), "the directory is exempt");
    }

    #[test]
    fn the_operator_is_never_reported_as_deaf() {
        // THIS ASSERTION REPLACES ITS OWN INVERSE. The version I shipped yesterday said
        // "the operator is a recipient like any other" and asserted they WERE checked. They
        // are not like any other: they are a human on Telegram with no session and no
        // listener file, so the check fired every single time and told the sender their
        // message "will not wake them" about the one recipient a phone push does wake.
        //
        // I only found it by reading my own output while replying to the operator. The test
        // did not catch it because the test had been written from the same wrong belief as
        // the code — which is the argument for asserting on the OBSERVABLE claim (is this
        // warning ever true of the operator?) rather than on the function's mechanics.
        assert!(addressable_peers("@operator").is_empty());
        assert!(addressable_peers("@Operator").is_empty(), "case must not defeat it");
        // ...and it must not swallow real peers travelling beside them.
        assert_eq!(addressable_peers("@operator__witty-bison-2"), vec!["witty-bison-2"]);
    }


    #[test]
    fn an_attended_operator_produces_no_notice() {
        // The normal case. A line printed at the end of EVERY turn becomes furniture, and
        // then it is unread on the one turn it matters — the same reason the lobby nudge
        // stays silent on @all and the duplicate warning no longer fires on every write.
        assert!(away_line(paos_operator::Mode::Attended).is_none());
    }

    #[test]
    fn an_away_operator_is_told_with_the_verbs_that_actually_reach_them() {
        // Measured 2026-08-01: 26 of 1,031 sessions had EVER reached the operator, and
        // ZERO used `bus blocked` — the mechanism the global policy named, which notifies
        // nobody. So this must carry the COMMAND and say what does not work.
        let n = away_line(paos_operator::Mode::Away).expect("away must produce a notice");
        assert!(n.contains("paos operator ask"), "must name the verb for a question");
        assert!(n.contains("paos operator say"), "must name the verb for an update");
        assert!(n.contains("bus blocked"), "must say the marker does not notify anyone");
    }

    #[test]
    fn autonomous_is_told_to_proceed_not_to_ask() {
        // MY OWN BUG, caught 10 minutes after shipping it: the first version printed the
        // same "go ask" text for both modes. Wrong twice — autonomous means proceed on
        // best judgment within policy, so asking defeats the mode, and `may_push` only
        // opens Telegram for Away, so the ask would queue and reach nobody. SKILL.md:399
        // already drew the distinction and the notice contradicted it.
        let n = away_line(paos_operator::Mode::Autonomous).expect("autonomous needs a line");
        assert!(n.contains("proceed on your own judgment"));
        assert!(!n.contains("paos operator ask"),
                "telling an autonomous session to ask contradicts the mode AND would not \
                 reach the operator");
        assert!(n.contains("paos operator say"), "volunteering an update is still right");
    }

}
