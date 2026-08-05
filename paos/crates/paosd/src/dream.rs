//! The nightly `dream`: mine recent Claude Code sessions into candidate memories.
//!
//! This existed in the Python `paos-operatord` and was lost when the daemon moved to
//! Rust — silently, because nothing checks that a background job still has a home. The
//! global agent policy still tells every session it runs nightly, so the store was
//! documented as self-maintaining while nothing maintained it. Restored here.
//!
//! Two properties are deliberate, both inherited from the Python version's hard-won
//! comments:
//!
//!   * **Its own thread.** The Python version originally ran this inside the delivery
//!     loop; one dream shells out per session (83 in a measured run) and it held
//!     escalations, the outbox and the bus mirror hostage the whole time. The operator's
//!     "I didn't get your reply" was that, not the network.
//!   * **Clock-gated, not just interval-gated.** It drives many LLM calls, so it runs in
//!     an overnight window rather than whenever 22 hours have elapsed — otherwise it
//!     eventually competes with active daytime sessions for the same machine.
//!
//! The dream itself is still Python (`paos memory dream`): it reads session transcripts
//! and drafts proposals, which is squarely the skill's job. This module is only the
//! scheduler.

use rusqlite::Connection;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often to check whether it is time. Cheap: two SQLite reads and one `date`.
const CHECK_EVERY_SECS: u64 = 600;
/// A session is BLOCKED on this: it has been told its fact is saved and cannot see it
/// until the drain runs. Seconds, not minutes.
const SPOOL_EVERY_SECS: u64 = 5;
/// At most one dream per day, even across daemon restarts.
const MIN_INTERVAL_SECS: i64 = 79_200; // 22h
const DEFAULT_HOUR_START: i64 = 3;
const DEFAULT_HOUR_END: i64 = 6;
const DEFAULT_LIMIT: i64 = 8;
const DEFAULT_SINCE: &str = "26h";
/// Where the last run is recorded. Persisted rather than held in memory: the Python
/// version kept it in a global, so a daemon restart inside the window re-armed it and it
/// could dream twice in one night.
const LAST_RUN_KEY: &str = "dream.last_run_epoch";
/// Set only when a run actually FINISHES. Recording the start is what stops a restart
/// loop from dreaming repeatedly, but it also means a run killed halfway looks exactly
/// like a run that completed. This second marker is the difference, and `paos doctor`
/// reports when starts stop being followed by finishes.
const LAST_OK_KEY: &str = "dream.last_ok_epoch";

pub fn spawn(conn: Arc<Mutex<Connection>>, embedder: Arc<dyn paos_memory::Embedder>) {
    // The spool gets its OWN fast loop. It was riding this thread's 10-MINUTE tick while
    // the CLI told the caller "paosd stores it within ~10s" — so a session wrote a fact,
    // was told it was saved, looked two minutes later and found nothing. vivid-falcon
    // caught it and named it exactly: the shim went from "fails loudly" to "claims
    // success and spools into a directory nothing ingests", which is the same class of
    // bug moved one layer down.
    //
    // Cheap enough to run often: a readdir on a directory that is empty almost always.
    {
        let c = Arc::clone(&conn);
        let e = Arc::clone(&embedder);
        std::thread::spawn(move || loop {
            drain_spool(&c, e.as_ref());
            std::thread::sleep(Duration::from_secs(SPOOL_EVERY_SECS));
        });
    }
    std::thread::spawn(move || loop {
        if let Err(e) = tick(&conn) {
            eprintln!("paosd: dream: {e}");
        }
        // Backups ride this thread deliberately: it is the ONLY timer in the daemon that
        // does not depend on operator mode or the Telegram channel being open. A backup
        // that stops because the phone is quiet is not a backup.
        maybe_backup(&conn);
        std::thread::sleep(Duration::from_secs(CHECK_EVERY_SECS));
    });
}

/// Ingest memory writes that could not reach the socket.
///
/// Every agent session runs sandboxed, and that sandbox permits writing under ~/.paos but
/// DENIES connecting to a unix socket. So `paos memory remember` worked from a terminal
/// and failed from every session on this machine — the reflex the whole system is built
/// on, broken for its only users, and reported as "paosd unavailable" rather than fixed.
///
/// A file is the one channel the sandbox allows through. The daemon remains the single
/// writer and keeps ownership of the embedding, which is what made direct SQLite writes
/// from Python unacceptable in the first place.
fn drain_spool(conn: &Mutex<Connection>, embedder: &dyn paos_memory::Embedder) {
    drain_spool_at(&paos_store::root().join("spool"), conn, embedder)
}

/// Apply one spooled bus write.
///
/// `Ok(true)` applied · `Ok(false)` malformed, quarantine it · `Err` transient, retry.
/// The three outcomes are distinct on purpose: quarantining a transient failure loses a
/// join, and retrying a malformed one forever wedges the drain behind it.
fn apply_bus_op(conn: &Mutex<Connection>, op: &str, v: &serde_json::Value)
    -> rusqlite::Result<bool>
{
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").trim();
    let b = |k: &str| v.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
    let now = now_iso();
    let (name, room) = (s("name"), s("room"));

    match op {
        "bus_join" => {
            if name.is_empty() || room.is_empty() {
                return Ok(false);
            }
            // `repo` is optional: spool entries written by an older CLI do not carry it,
            // and those must still apply rather than quarantine.
            let repo = v.get("repo").and_then(|r| r.as_str());
            let g = lock(conn);
            paos_presence::join_with_repo(&g, room, name, &now, repo)?;
            Ok(true)
        }
        "bus_leave" => {
            if name.is_empty() || room.is_empty() {
                return Ok(false);
            }
            let g = lock(conn);
            paos_presence::leave(&g, room, name)?;
            Ok(true)
        }
        "bus_send" => {
            let (sender, text) = (s("sender"), v.get("text").and_then(|x| x.as_str()).unwrap_or(""));
            // An empty room or sender would file the message where nobody reads it. Empty
            // TEXT is allowed: a wake carries no body.
            if room.is_empty() || sender.is_empty() {
                return Ok(false);
            }
            let target = match s("target") {
                "" => paos_bus::ALL,
                t => t,
            };
            let mut g = lock(conn);
            // This is the path almost every session actually takes — a sandboxed agent
            // cannot reach the unix socket, so its sends arrive here. Anything that must
            // happen on a send belongs inside `post`, not beside one of its two callers.
            paos_bus::post(&mut g, room, sender, target, text, &now, b("urgent"), b("ambient"))?;
            Ok(true)
        }
        // The listener's read receipt. Spooled rather than written directly, so the
        // single-writer invariant holds even for the always-on process.
        "bus_cursor" => {
            let member = s("member");
            let seq = v.get("seq").and_then(|x| x.as_i64()).unwrap_or(-1);
            if room.is_empty() || member.is_empty() || seq < 0 {
                return Ok(false);
            }
            let g = lock(conn);
            // MAX, never a plain assignment: spool entries are applied in directory
            // order, not send order, so an older receipt can arrive after a newer one.
            // Letting it win would rewind the cursor and re-deliver messages the session
            // has already acted on.
            g.execute(
                "INSERT INTO cursors(room, member, seq) VALUES(?1,?2,?3) \
                 ON CONFLICT(room, member) DO UPDATE SET seq = MAX(seq, excluded.seq)",
                rusqlite::params![room, member, seq],
            )?;
            Ok(true)
        }
        // Proof of life from an armed listener. Without it, an idle-but-listening
        // session takes no turns, its Stop-hook heartbeat never fires, and the reaper
        // archives it as dead — cascading its room memberships and making it deaf.
        "bus_touch" => {
            if name.is_empty() {
                return Ok(false);
            }
            let g = lock(conn);
            g.execute("UPDATE sessions SET last_seen=?1 WHERE name=?2 AND ended_ts IS NULL",
                      rusqlite::params![now, name])?;
            if !room.is_empty() {
                g.execute("UPDATE members SET last_seen=?1 WHERE room=?2 AND name=?3",
                          rusqlite::params![now, room, name])?;
            }
            Ok(true)
        }
        // Closing evicts every member but KEEPS the transcript. A room is closed because
        // its work is done, not because its history stopped mattering.
        "bus_close" => {
            if room.is_empty() { return Ok(false); }
            let g = lock(conn);
            g.execute("UPDATE rooms SET closed_ts=?1 WHERE room=?2",
                      rusqlite::params![now, room])?;
            g.execute("DELETE FROM members WHERE room=?1", [room])?;
            Ok(true)
        }
        "bus_topic" => {
            if room.is_empty() { return Ok(false); }
            let g = lock(conn);
            g.execute("INSERT OR IGNORE INTO rooms(room, created_ts) VALUES(?1,?2)",
                      rusqlite::params![room, now])?;
            g.execute("UPDATE rooms SET topic=?1 WHERE room=?2",
                      rusqlite::params![s("topic"), room])?;
            Ok(true)
        }
        "bus_kind" => {
            let kind = s("kind");
            if room.is_empty()
                || !paos_bus::readonly::ROOM_KINDS.iter().any(|(k, _)| *k == kind) {
                return Ok(false);
            }
            let g = lock(conn);
            g.execute("INSERT OR IGNORE INTO rooms(room, created_ts) VALUES(?1,?2)",
                      rusqlite::params![room, now])?;
            g.execute("UPDATE rooms SET kind=?1 WHERE room=?2",
                      rusqlite::params![kind, room])?;
            if !s("repos").is_empty() {
                g.execute("UPDATE rooms SET repos=?1 WHERE room=?2",
                          rusqlite::params![s("repos"), room])?;
            }
            Ok(true)
        }
        // Irreversible, unlike close: the transcript goes too. The CLI gates it behind
        // --force and shows the counts first.
        "bus_delete_room" => {
            if room.is_empty() { return Ok(false); }
            let g = lock(conn);
            for t in ["messages", "cursors", "members", "rooms"] {
                g.execute(&format!("DELETE FROM {t} WHERE room=?1"), [room])?;
            }
            Ok(true)
        }
        "bus_forget" => {
            if name.is_empty() { return Ok(false); }
            let g = lock(conn);
            g.execute("DELETE FROM members WHERE name=?1", [name])?;
            g.execute("DELETE FROM cursors WHERE member=?1", [name])?;
            g.execute("DELETE FROM sessions WHERE name=?1", [name])?;
            Ok(true)
        }
        // Supervisor sweeps. They run in the daemon because they are bulk WRITES and
        // because the pid check must happen where the process table is readable — a
        // sandboxed CLI cannot judge liveness for anything but itself.
        "bus_reap" => {
            let g = lock(conn);
            let reaped = paos_presence::reap_dead(&g, epoch_now())?;
            if !reaped.is_empty() {
                eprintln!("paosd: reaped {} dead session(s): {}", reaped.len(), reaped.join(", "));
            }
            Ok(true)
        }
        // The session lifecycle, spooled. The presence hook normally reaches the socket —
        // it runs in the harness, not the agent sandbox — so these arms are the safety
        // net for when it cannot. Losing a session-start means an unbound session; losing
        // a session-end leaves a dead session in the roster holding its memberships.
        "bus_session_start" => {
            let sid = s("session_id");
            if sid.is_empty() { return Ok(false); }
            let pid = v.get("pid").and_then(|x| x.as_i64());
            let g = lock(conn);
            paos_presence::session_start(&g, sid, s("name"), pid, &now)?;
            Ok(true)
        }
        "bus_heartbeat" => {
            let sid = s("session_id");
            if sid.is_empty() { return Ok(false); }
            let g = lock(conn);
            // `false` means no live session with that id — already ended, or never
            // started. Applied is applied; retrying forever would wedge the drain.
            let _ = paos_presence::heartbeat(&g, sid, v.get("pid").and_then(|x| x.as_i64()), &now)?;
            Ok(true)
        }
        "bus_session_end" => {
            let sid = s("session_id");
            if sid.is_empty() { return Ok(false); }
            let g = lock(conn);
            let _ = paos_presence::session_end(&g, sid, &now)?;
            Ok(true)
        }
        "bus_prune_rooms" => {
            let g = lock(conn);
            let (closed, purged) = paos_presence::prune_rooms(&g, epoch_now())?;
            let ghosts = paos_presence::purge_orphan_members(&g, epoch_now())?;
            eprintln!("paosd: room GC: closed={closed} purged={purged} ghost-members={ghosts}");
            Ok(true)
        }
        "bus_prune" => {
            let mins = v.get("older_than_min").and_then(|x| x.as_i64()).unwrap_or(60);
            let g = lock(conn);
            let pruned = paos_presence::prune_members(&g, epoch_now(), mins)?;
            if !pruned.is_empty() {
                eprintln!("paosd: pruned {} stale member(s)", pruned.len());
            }
            Ok(true)
        }
        // The session has been SHOWN the protocol drift. Recorded so the notice does not
        // repeat on every turn — an unheeded banner every turn is noise, and noise is how
        // a real one stops being read.
        // THE OPERATOR CHANNEL, SPOOLED. Every bus and memory write already degrades to
        // the spool when the socket is blocked; these did not, so `paos operator say|ask`
        // failed outright from inside an agent sandbox — which is where EVERY session
        // lives. Measured 2026-08-01: 26 of 1,031 sessions had ever reached the operator.
        // The channel for reaching a human was the only one that did not work from where
        // humans were being reached from.
        "operator_say" => {
            let (session, text) = (s("session"), s("text"));
            if text.trim().is_empty() { return Ok(true); }
            let g = lock(conn);
            paos_operator::enqueue_say(&g, session, text.trim(), &now)
                .map(|_| true)
                .map_err(|e| e.into())
        }
        "operator_ask" => {
            let (session, question) = (s("session"), s("question"));
            if question.trim().is_empty() { return Ok(true); }
            let opts = v.get("options").and_then(|o| o.as_str());
            let g = lock(conn);
            paos_operator::ask(&g, session, question.trim(), opts, &now)
                .map(|_| true)
                .map_err(|e| e.into())
        }
        "bus_ack_version" => {
            let v = s("version");
            if name.is_empty() || v.is_empty() { return Ok(false); }
            let g = lock(conn);
            g.execute("UPDATE sessions SET ack_skill_version=?1 WHERE name=?2",
                      rusqlite::params![v, name])?;
            Ok(true)
        }
        "bus_status" => {
            if name.is_empty() {
                return Ok(false);
            }
            // A missing/null `status` clears it — that is how `status --clear` travels.
            let status = v.get("status").and_then(|x| x.as_str()).map(str::trim)
                .filter(|x| !x.is_empty());
            let g = lock(conn);
            paos_presence::set_status(&g, name, status, &now)?;
            Ok(true)
        }
        // THE WORK QUEUE, SPOOLED. Without these arms every `paos task` write from a
        // session would fall through to `_ => Ok(false)` — quarantined as malformed and
        // renamed to `.bad`. Not an error anyone would see: the CLI prints "spooled" and
        // the task simply never appears. The socket is blocked in every agent sandbox, so
        // that is not an edge case, it is the normal path.
        "task_create" => {
            let title = s("title");
            if title.is_empty() { return Ok(false); }
            let Some(origin) = paos_tasks::model::Origin::parse(s("origin")) else {
                return Ok(false);
            };
            let opt = |k: &str| v.get(k).and_then(|x| x.as_str())
                .map(str::to_string).filter(|x| !x.is_empty());
            let n = paos_tasks::model::NewTask {
                title: title.to_string(),
                body: opt("body"),
                scope: s("scope").to_string(),
                org: opt("org"),
                repo: opt("repo"),
                parent_id: opt("parent_id"),
                priority: v.get("priority").and_then(|x| x.as_i64()).unwrap_or(2),
                origin,
                created_by: s("created_by").to_string(),
                room: opt("room"),
                start_ready: b("start_ready"),
            };
            let g = lock(conn);
            match paos_tasks::store::create(&g, &n, &now) {
                Ok(_) => Ok(true),
                // A rejected task (bad scope, missing parent) is malformed, not transient:
                // retrying forever would wedge every later entry behind it.
                Err(e) => { eprintln!("paosd: spool task_create rejected: {e}"); Ok(false) }
            }
        }
        "task_claim" => {
            let (id, session) = (s("id"), s("session"));
            if id.is_empty() || session.is_empty() { return Ok(false); }
            let mut g = lock(conn);
            // Losing the race is a legitimate outcome, not a failure to retry — the CLI
            // learns who won by reading the row back.
            match paos_tasks::store::claim(&mut g, id, session, &now) {
                Ok(_) => Ok(true),
                Err(e) => { eprintln!("paosd: spool task_claim failed: {e}"); Ok(false) }
            }
        }
        "task_release" => {
            let (id, session) = (s("id"), s("session"));
            if id.is_empty() || session.is_empty() { return Ok(false); }
            let g = lock(conn);
            match paos_tasks::store::release(&g, id, session, &now) {
                Ok(()) => Ok(true),
                // Same reasoning as task_state: refusing to release someone else's task is
                // the rule working, not a corrupt entry.
                Err(e) => { eprintln!("paosd: spool task_release refused: {e}"); Ok(true) }
            }
        }
        "task_state" => {
            let (id, actor) = (s("id"), s("actor"));
            let Some(to) = paos_tasks::model::State::parse(s("to")) else { return Ok(false) };
            if id.is_empty() { return Ok(false); }
            let a = if actor == "operator" { paos_tasks::store::Actor::Operator }
                    else { paos_tasks::store::Actor::Session(actor) };
            let g = lock(conn);
            match paos_tasks::store::set_state(&g, id, to, &a, &now) {
                Ok(()) => Ok(true),
                // A REFUSAL IS NOT A PARSE FAILURE. Quarantining one renames the entry to
                // .bad, which the doctor reports to the operator as "a write paosd could
                // not parse" — a page for a rule working exactly as designed. The CLI
                // pre-checks this before spooling, so reaching here means the state
                // changed underneath (a grant revoked, the task dropped); record it where
                // someone would actually look and consume the entry.
                Err(e) => {
                    let _ = paos_tasks::store::note(
                        &g, id, "paos", "state",
                        &format!("refused: {e}"), &now);
                    eprintln!("paosd: spool task_state refused: {e}");
                    Ok(true)
                }
            }
        }
        "task_note" => {
            let (id, author) = (s("id"), s("author"));
            let text = v.get("text").and_then(|x| x.as_str()).unwrap_or("");
            if id.is_empty() || text.trim().is_empty() { return Ok(false); }
            let g = lock(conn);
            paos_tasks::store::note(&g, id, author, "note", text, &now)
                .map(|_| true)
                .map_err(|e| rusqlite::Error::InvalidParameterName(e))
        }
        "task_grant" => {
            let id = s("id");
            if id.is_empty() { return Ok(false); }
            let g = lock(conn);
            match paos_tasks::store::grant_close(&g, id, &now) {
                Ok(()) => Ok(true),
                Err(e) => { eprintln!("paosd: spool task_grant failed: {e}"); Ok(false) }
            }
        }
        "task_dep" => {
            let (id, dep) = (s("id"), s("depends_on"));
            if id.is_empty() || dep.is_empty() { return Ok(false); }
            let g = lock(conn);
            let r = if b("remove") { paos_tasks::store::dep_rm(&g, id, dep) }
                    else { paos_tasks::store::dep_add(&g, id, dep, &now) };
            match r {
                Ok(()) => Ok(true),
                Err(e) => { eprintln!("paosd: spool task_dep refused: {e}"); Ok(true) }
            }
        }
        _ => Ok(false),
    }
}

/// The drain itself, against an explicit directory.
///
/// Split out so tests can point it at a tempdir. Reading the root from the environment
/// made this untestable except by mutating PAOS_ROOT, which races other tests in the same
/// process — and an untested drain is how the spool silently sat unread for 10 minutes at
/// a time while the CLI promised 10 seconds.
fn drain_spool_at(dir: &std::path::Path, conn: &Mutex<Connection>,
                  embedder: &dyn paos_memory::Embedder) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&path) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
            // Unparseable: rename rather than delete. Losing a fact silently is the
            // failure this whole mechanism exists to prevent.
            let _ = std::fs::rename(&path, path.with_extension("bad"));
            continue;
        };
        // Deletes ride the same channel. Without this a sandboxed session could add a
        // fact but never retract one, so a wrong memory was permanent for its only users.
        if v.get("op").and_then(|o| o.as_str()) == Some("forget") {
            let Some(id) = v.get("id").and_then(|i| i.as_str()).filter(|i| !i.is_empty())
            else {
                let _ = std::fs::rename(&path, path.with_extension("bad"));
                continue;
            };
            let res = { let g = lock(conn); paos_memory::forget(&g, id) };
            match res {
                // An id that is already gone is the goal state, not a failure to retry.
                Ok(_) => { let _ = std::fs::remove_file(&path); }
                Err(e) => eprintln!("paosd: spool forget {id} failed, keeping it: {e}"),
            }
            continue;
        }
        // Config writes ride the channel too. `paos config set` from inside a sandbox
        // otherwise had nowhere to go, and the Python's answer was to write the table
        // itself — the multi-writer arrangement this daemon exists to remove.
        if v.get("op").and_then(|o| o.as_str()) == Some("config_set") {
            let key = v.get("key").and_then(|k| k.as_str()).unwrap_or("");
            let value = v.get("value").and_then(|x| x.as_str()).unwrap_or("");
            if key.is_empty() {
                let _ = std::fs::rename(&path, path.with_extension("bad"));
                continue;
            }
            let res = {
                let g = lock(conn);
                g.execute(
                    "INSERT INTO paos_config(key,value,updated_ts) VALUES(?1,?2,?3) \
                     ON CONFLICT(key) DO UPDATE SET value=excluded.value, \
                     updated_ts=excluded.updated_ts",
                    rusqlite::params![key, value, now_iso()],
                )
            };
            match res {
                Ok(_) => { let _ = std::fs::remove_file(&path); }
                Err(e) => eprintln!("paosd: spool config {key} failed, keeping it: {e}"),
            }
            continue;
        }
        // Bus writes ride the channel too, and they are the reason the quarantine guard
        // below exists. Without these, a sandboxed session — which is every session — can
        // READ the bus but not join, leave, send or set a status, so `reachable` could
        // diagnose a dropped room and never repair it. The Python "solved" this by writing
        // SQLite from the CLI, which is the multi-writer arrangement this daemon removes.
        // BOTH prefixes dispatch here. operator_ was added on 2026-08-01 and the first
        // attempt put the handlers in apply_bus_op WITHOUT widening this filter — so they
        // were unreachable, and the op fell through to the MEMORY path below, where a
        // widened allowlist would have stored an operator message as a memory fact. That
        // is the exact failure the quarantine guard was written to stop.
        // `task_` joined them on 2026-08-02 and hit the same trap: the seven arms were
        // written in apply_bus_op first and this filter was left alone, so every task
        // write a session spooled would have fallen through to the memory path. The
        // parity test below caught it before it shipped, which is the second time it has
        // earned its keep.
        if let Some(op) = v.get("op").and_then(|o| o.as_str())
            .filter(|o| o.starts_with("bus_") || o.starts_with("operator_")
                     || o.starts_with("task_")) {
            match apply_bus_op(conn, op, &v) {
                Ok(true) => { let _ = std::fs::remove_file(&path); }
                // Malformed: quarantine. Retrying forever would wedge the drain, and a bus
                // write is not worth blocking every later spool entry behind it.
                Ok(false) => {
                    eprintln!("paosd: spool {}: malformed {op}, quarantined", path.display());
                    let _ = std::fs::rename(&path, path.with_extension("bad"));
                }
                // A transient database error: KEEP it and retry on the next tick. Dropping
                // a join here is how a session silently stops receiving room traffic.
                Err(e) => eprintln!("paosd: spool {op} failed, keeping it: {e}"),
            }
            continue;
        }
        // Review-queue writes ride the channel for the same reason config does: from
        // inside a sandbox they have nowhere else to go, and the Python's answer was to
        // write `memory_proposals` itself from every session at once.
        if v.get("op").and_then(|o| o.as_str()) == Some("proposal_add") {
            let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("");
            let dataset = v.get("dataset").and_then(|d| d.as_str()).unwrap_or("");
            if kind.is_empty() || dataset.is_empty() {
                eprintln!("paosd: spool {}: proposal_add needs kind and dataset",
                          path.display());
                let _ = std::fs::rename(&path, path.with_extension("bad"));
                continue;
            }
            let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
            let res = {
                let g = lock(conn);
                paos_librarian::queue::add(
                    &g, kind, dataset, s("text").as_deref(), s("scope").as_deref(),
                    s("target_data_id").as_deref(), s("rationale").as_deref(),
                    s("source").as_deref(), &now_iso(),
                )
            };
            match res {
                Ok(_) => { let _ = std::fs::remove_file(&path); }
                Err(e) => eprintln!("paosd: spool proposal_add failed, keeping it: {e}"),
            }
            continue;
        }
        if v.get("op").and_then(|o| o.as_str()) == Some("proposal_set_status") {
            let id = v.get("id").and_then(|i| i.as_i64());
            let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
            let Some(id) = id.filter(|_| status == "approved" || status == "rejected") else {
                eprintln!("paosd: spool {}: proposal_set_status needs an id and a status \
                           of approved or rejected", path.display());
                let _ = std::fs::rename(&path, path.with_extension("bad"));
                continue;
            };
            let res = {
                let g = lock(conn);
                paos_librarian::queue::set_status(&g, id, status, &now_iso())
            };
            match res {
                // `false` means it was already resolved — the goal state, not a retry.
                Ok(_) => { let _ = std::fs::remove_file(&path); }
                Err(e) => eprintln!("paosd: spool proposal_set_status failed, keeping: {e}"),
            }
            continue;
        }
        // A supersede is a write that also retires something, so it shares the write path
        // below and differs only in what happens after the store. Doing it the other way
        // — a separate branch — is how the two would drift on scope derivation, and a
        // replacement filed in a different dataset than its original is worse than none.
        let supersedes = v.get("op").and_then(|o| o.as_str()) == Some("supersede");
        // Everything past this point is the memory-write path, and it keys off `text`.
        // So an entry with an op nobody handled — a typo, or a verb from a facet being
        // ported that the daemon does not understand YET — was silently STORED AS A
        // MEMORY FACT in the global tier. A spooled bus message would have become a
        // memory. Quarantine instead: an unknown op is a bug to be seen, never a fact.
        if let Some(op) = v.get("op").and_then(|o| o.as_str()) {
            // BOTH migrations' ops, plus the bus_ prefix. This allowlist is the thing
            // that stops an unrecognised op being filed as a memory fact, so forgetting to
            // extend it quarantines a legitimate write instead — noisy, but never silent.
            if !matches!(op, "supersede" | "forget" | "config_set"
                            | "proposal_add" | "proposal_set_status")
                && !op.starts_with("bus_") && !op.starts_with("operator_")
                && !op.starts_with("task_") {
                // NOTE: operator_ and task_ are excluded here only because they are
                // dispatched ABOVE and never arrive. If that dispatch is ever narrowed,
                // these stop being no-ops and start filing operator messages and task
                // writes as memory facts.
                eprintln!("paosd: spool {}: unknown op {op:?}, quarantined", path.display());
                let _ = std::fs::rename(&path, path.with_extension("bad"));
                continue;
            }
        }
        let old_ids: Vec<String> = v.get("old_ids").and_then(|o| o.as_array())
            .map(|a| a.iter().filter_map(|i| i.as_str()).map(str::to_string).collect())
            .unwrap_or_default();
        if supersedes && old_ids.is_empty() {
            // Degrading to a plain write would leave the stale facts live forever, which
            // is precisely what the operator was correcting.
            let _ = std::fs::rename(&path, path.with_extension("bad"));
            continue;
        }
        let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("").trim().to_string();
        if text.is_empty() {
            let _ = std::fs::rename(&path, path.with_extension("bad"));
            continue;
        }
        let dataset = match v.get("dataset").and_then(|d| d.as_str()).filter(|d| !d.is_empty()) {
            Some(d) => d.to_string(),
            None => {
                let tier = match v.get("tier").and_then(|t| t.as_str()).unwrap_or("global") {
                    "org" => paos_memory::scope::Tier::Org,
                    "project" => paos_memory::scope::Tier::Project,
                    _ => paos_memory::scope::Tier::Global,
                };
                let origin = v.get("origin").and_then(|o| o.as_str())
                    .and_then(paos_memory::scope::parse_origin);
                // The CONFIGURED global. A spooled write that landed in the compiled-in
                // default would be filed where this machine never looks.
                let global = { let g = lock(conn); paos_memory::scope::global_dataset(&g) };
                match paos_memory::scope::write_scope(tier, origin.as_ref(), &global) {
                    Ok(ds) => ds,
                    Err(e) => {
                        eprintln!("paosd: spool {}: {e}", path.display());
                        let _ = std::fs::rename(&path, path.with_extension("bad"));
                        continue;
                    }
                }
            }
        };
        let stored = {
            let g = lock(conn);
            let id = paos_memory::remember(&g, embedder, &dataset, &text, &now_iso());
            // Retire the original only once its replacement is safely stored, and under
            // the same lock, so no reader ever sees both live or neither.
            if let (Ok(new_id), true) = (&id, supersedes) {
                for old in &old_ids {
                    let _ = paos_memory::supersede(&g, old, new_id);
                }
            }
            id
        };
        match stored {
            // Delete only after the write succeeded; a crash between the two replays the
            // fact, which is recoverable. The reverse loses it, which is not.
            Ok(_) => { let _ = std::fs::remove_file(&path); }
            Err(e) => eprintln!("paosd: spool {} failed, keeping it: {e}", path.display()),
        }
    }
}

/// One snapshot a day of the one file that cannot be rebuilt.
///
/// ~/.paos/paos.db holds every durable memory, bus message and session record on this
/// machine, and nothing backed it up — Time Machine has no destination configured. Shells
/// `paos backup run`, which does the consistent VACUUM INTO and verifies the copy; there
/// is no reason for a second implementation here.
fn maybe_backup(conn: &Mutex<Connection>) {
    const DAY: i64 = 86_400;
    const KEY: &str = "backup.last_ok_epoch";
    let now = now_epoch();
    {
        let g = lock(conn);
        let last: i64 = g
            .query_row("SELECT value FROM meta WHERE key=?1", [KEY], |r| r.get::<_, String>(0))
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        if now - last < DAY {
            return;
        }
    }
    // Called DIRECTLY, not by shelling the Python CLI. That indirection meant the only
    // backup of the one irreplaceable file on this machine depended on a Python import
    // succeeding — and would have stopped, silently, the moment it did not.
    let dest = paos_store::backup::default_dest();
    let out_path = dest.join(paos_store::backup::stamped_name(&utc_stamp()));
    match paos_store::backup::snapshot(&paos_store::db_path(), &out_path) {
        Ok(msg) => {
            let pruned = paos_store::backup::prune(&dest, paos_store::backup::KEEP_DAILY);
            // Recorded only on SUCCESS. Stamping it regardless would mean one failed
            // night silently costs a day of coverage, and the next check would report a
            // backup that does not exist.
            set_meta(conn, KEY, now);
            eprintln!("paosd: backup ok: {} -> {} ({msg}, pruned {pruned})",
                      paos_store::db_path().display(), out_path.display());
        }
        Err(e) => eprintln!("paosd: backup FAILED: {e}"),
    }
}

/// `YYYYMMDDTHHMMSSZ` for snapshot filenames.
fn utc_stamp() -> String {
    Command::new("date").args(["-u", "+%Y%m%dT%H%M%SZ"]).output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string()).unwrap_or_default()
}

fn tick(conn: &Mutex<Connection>) -> Result<(), String> {
    // Config first — it is the cheapest way to say no, and the operator explicitly
    // disabled this at one point. A scheduler that ignores its off switch is worse than
    // no scheduler.
    if !enabled(conn) {
        return Ok(());
    }
    let now = now_epoch();
    if now - last_run(conn) < MIN_INTERVAL_SECS {
        return Ok(());
    }
    let hour = local_hour().ok_or("cannot read the local hour")?;
    let (start, end) = (
        cfg_int(conn, "dream_hour_start", DEFAULT_HOUR_START),
        cfg_int(conn, "dream_hour_end", DEFAULT_HOUR_END),
    );
    if hour < start || hour >= end {
        return Ok(());
    }

    // Record BEFORE running, not after. A dream can take many minutes; if the daemon is
    // restarted mid-run, an unrecorded start means it fires again immediately.
    set_last_run(conn, now);

    let limit = cfg_int(conn, "dream_nightly_limit", DEFAULT_LIMIT).to_string();
    let since = cfg_str(conn, "dream_nightly_since", DEFAULT_SINCE);
    let paos = paos_skill();
    eprintln!("paosd: nightly dream starting (since {since}, limit {limit})");
    match Command::new(&paos)
        .args(["memory", "dream", "--since", &since, "--limit", &limit])
        .output()
    {
        Ok(out) => {
            let tail = String::from_utf8_lossy(&out.stdout);
            eprintln!(
                "paosd: nightly dream finished ({}): {}",
                out.status,
                tail.lines().last().unwrap_or("(no output)")
            );
            if out.status.success() {
                set_meta(conn, LAST_OK_KEY, now_epoch());
            }
        }
        // Never propagate: a failed dream must not take the daemon with it.
        Err(e) => eprintln!("paosd: nightly dream could not start {paos}: {e}"),
    }
    Ok(())
}

/// The paos skill entry point. Deployed to ~/.claude/skills/paos/paos by phase 20.
fn paos_skill() -> String {
    std::env::var("PAOS_SKILL_BIN").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/.claude/skills/paos/paos")
    })
}

/// Local hour via `date`. There is no chrono here, and shelling out gets the timezone
/// AND daylight saving right for free — 144 invocations a day is not worth a dependency
/// or a hand-rolled offset table.
fn local_hour() -> Option<i64> {
    let out = Command::new("date").arg("+%H").output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Seconds since the epoch, for the supervisor sweeps.
fn epoch_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_iso() -> String {
    crate::handlers::now_iso()
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn lock(c: &Mutex<Connection>) -> std::sync::MutexGuard<'_, Connection> {
    c.lock().unwrap_or_else(|p| p.into_inner())
}

/// Default ON, matching the Python — but the stored config wins when present.
fn enabled(conn: &Mutex<Connection>) -> bool {
    match cfg_raw(conn, "dream_enabled") {
        Some(v) => paos_memory::doctor::is_truthy(&v),
        None => true,
    }
}

fn cfg_raw(conn: &Mutex<Connection>, key: &str) -> Option<String> {
    lock(conn)
        .query_row("SELECT value FROM paos_config WHERE key=?1", [key], |r| {
            r.get::<_, String>(0)
        })
        .ok()
}

fn cfg_int(conn: &Mutex<Connection>, key: &str, fallback: i64) -> i64 {
    cfg_raw(conn, key)
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(fallback)
}

fn cfg_str(conn: &Mutex<Connection>, key: &str, fallback: &str) -> String {
    cfg_raw(conn, key)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

fn last_run(conn: &Mutex<Connection>) -> i64 {
    lock(conn)
        .query_row("SELECT value FROM meta WHERE key=?1", [LAST_RUN_KEY], |r| {
            r.get::<_, String>(0)
        })
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

fn set_last_run(conn: &Mutex<Connection>, epoch: i64) {
    set_meta(conn, LAST_RUN_KEY, epoch);
}

fn set_meta(conn: &Mutex<Connection>, key: &str, epoch: i64) {
    let _ = lock(conn).execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
        rusqlite::params![key, epoch.to_string()],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Arc<Mutex<Connection>> {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE paos_config(key TEXT PRIMARY KEY, value TEXT, updated_ts TEXT);
             CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        Arc::new(Mutex::new(c))
    }

    fn put(c: &Mutex<Connection>, k: &str, v: &str) {
        lock(c)
            .execute(
                "INSERT OR REPLACE INTO paos_config(key,value) VALUES(?1,?2)",
                [k, v],
            )
            .unwrap();
    }

    #[test]
    fn defaults_to_on_when_unconfigured() {
        assert!(enabled(&db()));
    }

    #[test]
    fn the_operators_off_switch_is_respected() {
        // He set dream_enabled=0 on 2026-07-29. A scheduler that ignores that is worse
        // than no scheduler.
        let c = db();
        put(&c, "dream_enabled", "0");
        assert!(!enabled(&c));
        put(&c, "dream_enabled", "off");
        assert!(!enabled(&c));
        put(&c, "dream_enabled", "1");
        assert!(enabled(&c));
    }

    #[test]
    fn a_started_run_and_a_finished_run_are_recorded_separately() {
        // A dream takes tens of minutes. Killed halfway — a daemon upgrade will do it —
        // it must not be indistinguishable from one that completed.
        let c = db();
        set_last_run(&c, 100);
        assert_eq!(last_run(&c), 100);
        assert_eq!(
            lock(&c).query_row("SELECT count(*) FROM meta WHERE key=?1", [LAST_OK_KEY],
                               |r| r.get::<_, i64>(0)).unwrap(),
            0,
            "starting must not imply finishing"
        );
        set_meta(&c, LAST_OK_KEY, 200);
        assert_eq!(
            lock(&c).query_row("SELECT value FROM meta WHERE key=?1", [LAST_OK_KEY],
                               |r| r.get::<_, String>(0)).unwrap(),
            "200"
        );
    }

    #[test]
    fn last_run_survives_a_restart() {
        // Held in a global by the Python version, so a restart inside the overnight
        // window could dream twice in one night.
        let c = db();
        assert_eq!(last_run(&c), 0);
        set_last_run(&c, 1_785_000_000);
        assert_eq!(last_run(&c), 1_785_000_000);
    }

    #[test]
    fn config_overrides_the_window_and_falls_back_when_absent() {
        let c = db();
        assert_eq!(cfg_int(&c, "dream_hour_start", DEFAULT_HOUR_START), 3);
        put(&c, "dream_hour_start", "1");
        assert_eq!(cfg_int(&c, "dream_hour_start", DEFAULT_HOUR_START), 1);
        // Garbage must not silently become 0 and open the window all night.
        put(&c, "dream_hour_start", "not-a-number");
        assert_eq!(cfg_int(&c, "dream_hour_start", DEFAULT_HOUR_START), 3);
    }

    #[test]
    fn an_empty_since_falls_back_rather_than_passing_nothing() {
        let c = db();
        put(&c, "dream_nightly_since", "   ");
        assert_eq!(cfg_str(&c, "dream_nightly_since", DEFAULT_SINCE), "26h");
    }

    #[test]
    fn the_local_hour_is_readable() {
        let h = local_hour().expect("date +%H must work");
        assert!((0..24).contains(&h), "got {h}");
    }

    // ---- the spool drain ----
    //
    // This mechanism carries EVERY memory write made by every sandboxed session on this
    // machine, and had no test at all. Two of the day's failures lived here: writes that
    // sat unread, and deletes that had no path through it whatsoever.

    fn memdb() -> Arc<Mutex<Connection>> {
        let c = Connection::open_in_memory().unwrap();
        paos_memory::ensure_schema(&c).unwrap();
        Arc::new(Mutex::new(c))
    }

    fn spool(dir: &std::path::Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    fn count(c: &Mutex<Connection>) -> i64 {
        lock(c).query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0)).unwrap()
    }

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("paos-drain-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A store with BOTH schemas, as the real one has. The bus tables alone would let
    /// `a_bus_op_never_becomes_a_memory` pass for the wrong reason — there would be no
    /// `memories` table for a leak to land in.
    fn busdb() -> Arc<Mutex<Connection>> {
        let c = paos_store::open_in_memory().unwrap();
        paos_memory::ensure_schema(&c).unwrap();
        Arc::new(Mutex::new(c))
    }

    #[test]
    fn a_spooled_join_lands_so_a_sandboxed_session_can_repair_itself() {
        // The whole point: `reachable` inside a sandbox detects a dropped room but cannot
        // write. Without this the self-heal diagnoses and never heals, which is the state
        // that left a live session unreachable for six minutes.
        let d = tmpdir("busjoin");
        let c = busdb();
        spool(&d, "1.json", r#"{"op":"bus_join","room":"lobby","name":"witty-bison-2"}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        let rooms = paos_bus::joined_rooms(&lock(&c), "witty-bison-2").unwrap();
        assert_eq!(rooms, vec!["lobby"]);
        assert!(!d.join("1.json").exists(), "an applied entry is removed");
        assert!(!d.join("1.bad").exists(), "and not quarantined");
    }

    #[test]
    fn a_spooled_send_reaches_the_room_with_its_wake_flags_intact() {
        // urgent/ambient decide whether a peer is WOKEN. Losing them in the spool would
        // silently downgrade an urgent message to one that arrives whenever.
        let d = tmpdir("bussend");
        let c = busdb();
        spool(&d, "1.json",
              r#"{"op":"bus_send","room":"lobby","sender":"me","target":"@you","text":"hi","urgent":true}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        let msgs = paos_bus::unread(&lock(&c), "lobby", 0).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text, "hi");
        assert_eq!(msgs[0].target, "@you");
        assert!(msgs[0].urgent, "urgent must survive the spool");
        assert!(!msgs[0].ambient);
    }

    #[test]
    fn a_spooled_send_with_no_target_goes_to_the_whole_room() {
        let d = tmpdir("bussendall");
        let c = busdb();
        spool(&d, "1.json", r#"{"op":"bus_send","room":"lobby","sender":"me","text":"x"}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        let msgs = paos_bus::unread(&lock(&c), "lobby", 0).unwrap();
        assert_eq!(msgs[0].target, "@all", "an empty target must not address nobody");
    }

    #[test]
    fn a_spooled_status_sets_and_clears() {
        let d = tmpdir("busstatus");
        let c = busdb();
        {
            let g = lock(&c);
            paos_presence::session_start(&g, "sid", "me", None, "t").unwrap();
        }
        spool(&d, "1.json", r#"{"op":"bus_status","name":"me","status":"porting the bus"}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        assert_eq!(paos_bus::readonly::get_status(&lock(&c), "me"), "porting the bus");

        // A null status is how `--clear` travels; it must clear, not store "null".
        spool(&d, "2.json", r#"{"op":"bus_status","name":"me","status":null}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        assert_eq!(paos_bus::readonly::get_status(&lock(&c), "me"), "");
    }

    #[test]
    fn a_malformed_bus_op_is_quarantined_but_a_wake_with_no_body_is_not() {
        let d = tmpdir("busbad");
        let c = busdb();
        // No room: the message would go where nobody reads it.
        spool(&d, "1.json", r#"{"op":"bus_send","sender":"me","text":"orphan"}"#);
        // No name: nothing to join.
        spool(&d, "2.json", r#"{"op":"bus_join","room":"lobby"}"#);
        // Empty TEXT is legitimate — a wake carries no body — so this one must APPLY.
        spool(&d, "3.json", r#"{"op":"bus_send","room":"lobby","sender":"me","text":""}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        assert!(d.join("1.bad").exists(), "a send with no room is quarantined");
        assert!(d.join("2.bad").exists(), "a join with no name is quarantined");
        assert!(!d.join("3.bad").exists(), "an empty-bodied wake is valid");
        assert_eq!(paos_bus::unread(&lock(&c), "lobby", 0).unwrap().len(), 1);
    }

    #[test]
    fn the_hook_lifecycle_round_trips_through_the_spool() {
        // The exact three calls hooks/session-presence makes, in order, as they arrive
        // when the socket is blocked. This is the one path where a mistake is fleet-wide
        // and instant, so it is exercised end to end rather than per-op.
        let d = tmpdir("hooklife");
        let c = busdb();
        // A handle is MINTED — the hook passes none. An empty name here would create an
        // unaddressable session that collides with the next one.
        spool(&d, "1.json", r#"{"op":"bus_session_start","session_id":"sid-h","name":"","pid":4242}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        let (name, pid): (String, i64) = lock(&c).query_row(
            "SELECT name, pid FROM sessions WHERE session_id='sid-h'", [],
            |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert!(!name.is_empty() && name.contains('-'), "minted a real handle: {name}");
        assert_eq!(pid, 4242, "the ppid is the reaper's primary liveness signal");

        // Heartbeat advances last_seen; without it an idle session is reaped as dead.
        spool(&d, "2.json", r#"{"op":"bus_heartbeat","session_id":"sid-h"}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        let ended: Option<String> = lock(&c).query_row(
            "SELECT ended_ts FROM sessions WHERE session_id='sid-h'", [], |r| r.get(0)).unwrap();
        assert!(ended.is_none(), "a heartbeat must not retire the session");

        // SessionEnd archives AND cascades membership.
        lock(&c).execute("INSERT INTO members(room,name) VALUES('lobby',?1)", [&name]).unwrap();
        spool(&d, "3.json", r#"{"op":"bus_session_end","session_id":"sid-h"}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        let g = lock(&c);
        let ended: Option<String> = g.query_row(
            "SELECT ended_ts FROM sessions WHERE session_id='sid-h'", [], |r| r.get(0)).unwrap();
        assert!(ended.is_some(), "the session is archived");
        let members: i64 = g.query_row("SELECT COUNT(*) FROM members WHERE name=?1", [&name],
                                       |r| r.get(0)).unwrap();
        assert_eq!(members, 0, "session-end cascades membership");
        assert!(!d.join("1.bad").exists() && !d.join("2.bad").exists() && !d.join("3.bad").exists());
    }

    #[test]
    fn closing_a_room_evicts_members_but_keeps_the_transcript() {
        // A room is closed because its work finished, not because its history stopped
        // mattering — peers read closed rooms to reconstruct decisions.
        let d = tmpdir("busclose");
        let c = busdb();
        {
            let mut g = lock(&c);
            paos_presence::join(&g, "done", "me", "t").unwrap();
            paos_bus::post(&mut g, "done", "me", "@all", "a decision", "t", false, false).unwrap();
        }
        spool(&d, "1.json", r#"{"op":"bus_close","room":"done"}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        let g = lock(&c);
        let members: i64 = g.query_row("SELECT COUNT(*) FROM members WHERE room='done'", [],
                                       |r| r.get(0)).unwrap();
        let msgs: i64 = g.query_row("SELECT COUNT(*) FROM messages WHERE room='done'", [],
                                    |r| r.get(0)).unwrap();
        let closed: i64 = g.query_row(
            "SELECT COUNT(*) FROM rooms WHERE room='done' AND closed_ts IS NOT NULL", [],
            |r| r.get(0)).unwrap();
        assert_eq!((members, msgs, closed), (0, 1, 1), "evicted, kept, closed");
    }

    #[test]
    fn deleting_a_room_takes_the_transcript_too() {
        // The difference from close, and why the CLI gates it behind --force.
        let d = tmpdir("busdel");
        let c = busdb();
        {
            let mut g = lock(&c);
            paos_presence::join(&g, "gone", "me", "t").unwrap();
            paos_bus::post(&mut g, "gone", "me", "@all", "x", "t", false, false).unwrap();
        }
        spool(&d, "1.json", r#"{"op":"bus_delete_room","room":"gone"}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        let g = lock(&c);
        for (t, col) in [("messages", "room"), ("members", "room"), ("rooms", "room"),
                         ("cursors", "room")] {
            let n: i64 = g.query_row(&format!("SELECT COUNT(*) FROM {t} WHERE {col}='gone'"),
                                     [], |r| r.get(0)).unwrap();
            assert_eq!(n, 0, "{t} must be emptied");
        }
    }

    #[test]
    fn an_unknown_room_kind_is_refused_rather_than_stored() {
        // The kind decides the auto-close budget, so an unrecognised one would silently
        // fall back to the 2-day `task` lifetime and take a standing room with it.
        let d = tmpdir("buskind");
        let c = busdb();
        spool(&d, "1.json", r#"{"op":"bus_kind","room":"r","kind":"nonsense"}"#);
        spool(&d, "2.json", r#"{"op":"bus_kind","room":"r2","kind":"fleet"}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        assert!(d.join("1.bad").exists(), "an unknown kind is quarantined");
        let k: String = lock(&c).query_row("SELECT kind FROM rooms WHERE room='r2'", [],
                                           |r| r.get(0)).unwrap();
        assert_eq!(k, "fleet");
    }

    #[test]
    fn a_bus_op_never_becomes_a_memory() {
        // The guard and the handler must agree. If `bus_` stopped matching a branch, the
        // text-keyed memory path below would file the message as a durable fact.
        let d = tmpdir("busnotmem");
        let c = busdb();
        spool(&d, "1.json",
              r#"{"op":"bus_send","room":"lobby","sender":"me","text":"not a memory"}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        let n: i64 = lock(&c).query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "a bus message must never land in the memory store");
    }

    #[test]
    fn an_unknown_op_is_quarantined_not_stored_as_a_memory() {
        // The memory-write path keys off `text`, so before the guard ANY entry with an
        // unrecognised op and a text field was silently filed as a global memory fact. A
        // bus message spooled by a facet mid-port would have become a memory — a silent
        // corruption in the one place that is supposed to be durable truth.
        let d = tmpdir("unknownop");
        let c = memdb();
        spool(&d, "1.json", r#"{"op":"bus_send","room":"lobby","text":"hi there"}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        assert_eq!(count(&c), 0, "an unknown op must never become a memory");
        assert!(d.join("1.bad").exists(), "it must be quarantined for a human to see");
        assert!(!d.join("1.json").exists());
    }

    /// The proposal queue lives outside `paos_memory::ensure_schema`, so drain tests
    /// touching it need the table. Takes no PAOS_ROOT: tests are threads in one process.
    fn memdb_with_queue() -> Arc<Mutex<Connection>> {
        let c = Connection::open_in_memory().unwrap();
        paos_memory::ensure_schema(&c).unwrap();
        c.execute_batch(
            "CREATE TABLE memory_proposals(
               id INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT NOT NULL, dataset TEXT NOT NULL,
               scope TEXT, text TEXT, target_data_id TEXT, rationale TEXT, source TEXT,
               status TEXT NOT NULL DEFAULT 'pending', created_ts TEXT NOT NULL,
               resolved_ts TEXT, screen TEXT, screen_why TEXT);",
        )
        .unwrap();
        Arc::new(Mutex::new(c))
    }

    fn proposals(c: &Mutex<Connection>) -> Vec<(i64, String, String, Option<String>)> {
        let g = lock(c);
        let mut st = g
            .prepare("SELECT id, kind, status, screen FROM memory_proposals ORDER BY id")
            .unwrap();
        let v = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        v
    }

    #[test]
    fn a_spooled_proposal_lands_in_the_queue() {
        let d = tmpdir("proposal-add");
        let c = memdb_with_queue();
        spool(&d, "1.json", r#"{"op":"proposal_add","kind":"capture","dataset":"ds",
                                "text":"a durable fact","scope":"project","source":"dream"}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        let p = proposals(&c);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].1, "capture");
        assert_eq!(p[0].2, "pending");
        assert_eq!(count(&c), 0, "a proposal is NOT a memory — it awaits a human");
        assert!(!d.join("1.json").exists());
    }

    #[test]
    fn a_spooled_proposal_is_screened_but_never_rejected() {
        let d = tmpdir("proposal-screen");
        let c = memdb_with_queue();
        spool(&d, "1.json",
              r#"{"op":"proposal_add","kind":"capture","dataset":"ds","text":"all tests pass"}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        let p = proposals(&c);
        assert_eq!(p[0].3.as_deref(), Some("noise"), "flagged");
        assert_eq!(p[0].2, "pending", "advisory — flagged is NOT rejected");
    }

    #[test]
    fn a_spooled_status_change_resolves_the_proposal() {
        let d = tmpdir("proposal-status");
        let c = memdb_with_queue();
        let id = {
            let g = lock(&c);
            paos_librarian::queue::add(&g, "capture", "ds", Some("f"), None, None, None,
                                       None, "T").unwrap()
        };
        spool(&d, "1.json",
              &format!(r#"{{"op":"proposal_set_status","id":{id},"status":"approved"}}"#));
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        assert_eq!(proposals(&c)[0].2, "approved");
        assert!(!d.join("1.json").exists());
    }

    #[test]
    fn a_spooled_status_change_to_a_bogus_status_is_quarantined() {
        // Never guess. "deleted" is not a status this queue has, and applying it would
        // make the row invisible to both list_pending and the resolved views.
        let d = tmpdir("proposal-bogus");
        let c = memdb_with_queue();
        spool(&d, "1.json", r#"{"op":"proposal_set_status","id":1,"status":"deleted"}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        assert!(d.join("1.bad").exists(), "quarantined for a human to see");
    }

    #[test]
    fn a_spooled_proposal_without_a_kind_is_quarantined_not_stored_as_a_fact() {
        // The failure this guards: everything past the op dispatch keys off `text`, so an
        // entry the daemon does not fully understand used to be STORED AS A MEMORY.
        let d = tmpdir("proposal-nokind");
        let c = memdb_with_queue();
        spool(&d, "1.json", r#"{"op":"proposal_add","dataset":"ds","text":"not a fact"}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        assert_eq!(count(&c), 0, "must never become a memory");
        assert!(proposals(&c).is_empty());
        assert!(d.join("1.bad").exists());
    }

    #[test]
    fn the_new_ops_are_on_the_quarantine_allowlist() {
        // The allowlist is a second place every new op must be registered, and forgetting
        // it sends a perfectly good spooled write to .bad with only a stderr line.
        let d = tmpdir("proposal-allowlist");
        let c = memdb_with_queue();
        spool(&d, "1.json",
              r#"{"op":"proposal_add","kind":"lesson","dataset":"ds","text":"a trap"}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        assert!(!d.join("1.bad").exists(), "proposal_add must not be an unknown op");
        assert_eq!(proposals(&c).len(), 1);
    }

    #[test]
    fn a_spooled_write_lands_and_the_file_is_removed() {
        let d = tmpdir("write");
        let c = memdb();
        spool(&d, "1.json", r#"{"tier":"global","text":"a spooled fact"}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        assert_eq!(count(&c), 1);
        assert!(!d.join("1.json").exists(), "a drained entry must not replay forever");
    }

    #[test]
    fn a_spooled_forget_deletes_the_fact() {
        // The gap this exists for: `forget` was socket-only, so a sandboxed session could
        // add a wrong fact and was structurally unable to retract it.
        let d = tmpdir("forget");
        let c = memdb();
        let id = {
            let g = lock(&c);
            paos_memory::remember(&g, &paos_memory::HashEmbedder::new(64),
                                  "glob_test", "regrettable", "2026-07-31T00:00:00Z").unwrap()
        };
        assert_eq!(count(&c), 1);
        spool(&d, "1.json", &format!(r#"{{"op":"forget","id":"{id}"}}"#));
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        assert_eq!(count(&c), 0, "the spooled delete must actually delete");
        assert!(!d.join("1.json").exists());
    }

    fn live(c: &Mutex<Connection>) -> i64 {
        lock(c).query_row("SELECT COUNT(*) FROM memories WHERE superseded IS NULL", [],
                          |r| r.get(0)).unwrap()
    }

    #[test]
    fn a_spooled_supersede_retires_the_old_fact_without_destroying_it() {
        let d = tmpdir("sup");
        let c = memdb();
        let e = paos_memory::HashEmbedder::new(64);
        let old = {
            let g = lock(&c);
            paos_memory::remember(&g, &e, "glob_test", "the old claim", "2026-07-01T00:00:00Z").unwrap()
        };
        spool(&d, "1.json", &format!(
            r#"{{"op":"supersede","old_ids":["{old}"],"tier":"global","text":"the corrected claim"}}"#));
        drain_spool_at(&d, &c, &e);
        // Both rows survive — that is the difference from forget, and the whole reason
        // the column exists.
        assert_eq!(count(&c), 2, "supersede must not delete the original");
        assert_eq!(live(&c), 1, "the retired fact must stop being recallable");
    }

    #[test]
    fn a_supersede_lands_in_the_same_dataset_a_plain_write_would() {
        // A replacement filed somewhere other than its original is worse than none.
        let d = tmpdir("supds");
        let c = memdb();
        spool(&d, "1.json",
              r#"{"op":"supersede","old_ids":["whatever"],"dataset":"proj_a_b","text":"new"}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        let ds: String = lock(&c)
            .query_row("SELECT dataset FROM memories", [], |r| r.get(0)).unwrap();
        assert_eq!(ds, "proj_a_b");
    }

    #[test]
    fn a_supersede_missing_the_old_id_is_quarantined_not_stored_as_a_plain_fact() {
        // Silently degrading to a write would leave the stale fact live forever, which
        // is the failure the operator was explicitly trying to correct.
        let d = tmpdir("supbad");
        let c = memdb();
        spool(&d, "1.json", r#"{"op":"supersede","tier":"global","text":"new"}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        assert_eq!(count(&c), 0);
        assert!(d.join("1.bad").exists());
    }

    #[test]
    fn a_supersede_whose_target_is_already_gone_still_stores_the_replacement() {
        let d = tmpdir("supgone");
        let c = memdb();
        spool(&d, "1.json",
              r#"{"op":"supersede","old_ids":["never-existed"],"tier":"global","text":"new"}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        assert_eq!(live(&c), 1, "losing the replacement would be the worse outcome");
        assert!(!d.join("1.json").exists());
    }

    #[test]
    fn a_spooled_config_set_lands_in_the_table() {
        // `paos config set` from inside a sandbox had nowhere to go; the Python's answer
        // was to write paos_config itself, alongside the daemon.
        let d = tmpdir("cfg");
        let c = memdb();
        lock(&c).execute_batch(
            "CREATE TABLE IF NOT EXISTS paos_config(key TEXT PRIMARY KEY, value TEXT, \
             updated_ts TEXT)").unwrap();
        spool(&d, "1.json", r#"{"op":"config_set","key":"dream_hour_start","value":"4"}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        let v: String = lock(&c)
            .query_row("SELECT value FROM paos_config WHERE key='dream_hour_start'", [],
                       |r| r.get(0)).unwrap();
        assert_eq!(v, "4");
        assert!(!d.join("1.json").exists());
    }

    #[test]
    fn a_config_set_without_a_key_is_quarantined_not_stored_as_a_fact() {
        let d = tmpdir("cfgbad");
        let c = memdb();
        spool(&d, "1.json", r#"{"op":"config_set","value":"4"}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        assert_eq!(count(&c), 0, "a malformed config write must not become a memory");
        assert!(d.join("1.bad").exists());
    }

    #[test]
    fn a_forget_for_an_unknown_id_is_done_not_retried_forever() {
        // Already-gone is the goal state. Keeping the file would replay it every 5s.
        let d = tmpdir("gone");
        let c = memdb();
        spool(&d, "1.json", r#"{"op":"forget","id":"never-existed"}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        assert!(!d.join("1.json").exists());
    }

    #[test]
    fn a_forget_without_an_id_is_quarantined_not_dropped() {
        let d = tmpdir("noid");
        let c = memdb();
        spool(&d, "1.json", r#"{"op":"forget"}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        assert!(!d.join("1.json").exists());
        assert!(d.join("1.bad").exists(), "malformed entries are kept for inspection");
    }

    #[test]
    fn unparseable_and_empty_entries_are_never_silently_discarded() {
        let d = tmpdir("bad");
        let c = memdb();
        spool(&d, "1.json", "{not json");
        spool(&d, "2.json", r#"{"tier":"global","text":"   "}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        assert_eq!(count(&c), 0);
        assert!(d.join("1.bad").exists() && d.join("2.bad").exists());
    }

    #[test]
    fn non_json_files_in_the_spool_are_left_alone() {
        // .bad files from a previous drain must not be re-read on every pass.
        let d = tmpdir("skip");
        let c = memdb();
        spool(&d, "1.bad", "{not json");
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        assert!(d.join("1.bad").exists());
    }

    #[test]
    fn every_op_the_cli_can_spool_is_admitted_by_the_drain() {
        // The allowlist above and the CLI's emitters are two lists that must agree, in
        // two different crates, with nothing connecting them. Add a spooled op and forget
        // the allowlist and the write is quarantined: it exits 0, the caller is told
        // "spooled — paosd applies it within ~5s", and it never applies. Silent loss of a
        // bus message or a memory fact.
        //
        // Derived from the sources rather than restated, because the failure mode is
        // omission — the same reason the OWN_PARSER test now reads its constant out of
        // the source and the paos dispatcher's DELEGATED is checked against the branches.
        let cli = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../paos-cli/src");
        let mut emitted: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&cli).expect("paos-cli/src is readable") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let src = std::fs::read_to_string(&path).expect("readable source");
            // `"op": "<name>"` as it appears in the json! payloads the CLI spools.
            for (i, _) in src.match_indices("\"op\"") {
                let rest = &src[i + 4..];
                let Some(c) = rest.find(':') else { continue };
                let after = rest[c + 1..].trim_start();
                if !after.starts_with('"') {
                    continue;
                }
                let Some(end) = after[1..].find('"') else { continue };
                let op = &after[1..1 + end];
                if !op.is_empty() && op.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
                    emitted.push(op.to_string());
                }
            }
        }
        emitted.sort();
        emitted.dedup();
        assert!(emitted.len() >= 15,
                "found only {} spooled ops — the scan probably stopped matching, which \
                 would make this test vacuous: {emitted:?}", emitted.len());

        let src = include_str!("dream.rs");
        let allow = src
            .split("if !matches!(op,")
            .nth(1)
            .expect("the allowlist")
            .split("&& !op.starts_with")
            .next()
            .expect("its arms");
        for op in &emitted {
            // Prefixes must match the allowlist's own `starts_with` arms, or this test
            // reports a false quarantine — which is how it earned its keep on 2026-08-01,
            // catching operator_ask the moment the CLI learned to spool it.
            let admitted = op.starts_with("bus_") || op.starts_with("operator_")
                || op.starts_with("task_")
                || allow.contains(&format!("\"{op}\""));
            assert!(admitted,
                    "the CLI can spool {op:?} but the drain would QUARANTINE it — add it \
                     to the allowlist in drain_spool_at, or the write is lost silently");
        }
    }

    #[test]
    fn a_spooled_operator_message_never_becomes_a_memory_fact() {
        // THE BUG I NEARLY SHIPPED. The handlers were added to apply_bus_op without
        // widening the dispatch filter, so operator_ ops were unreachable and fell through
        // to the MEMORY path — where a widened allowlist would have stored the operator's
        // message as a global fact. Both halves had to be wrong together, which is exactly
        // why this asserts the OUTCOME rather than either half.
        let d = tmpdir("operatorsay");
        let c = busdb();
        let before: i64 = lock(&c)
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0)).unwrap();
        spool(&d, "1.json", r#"{"op":"operator_say","session":"s1","text":"reach the human"}"#);
        drain_spool_at(&d, &c, &paos_memory::HashEmbedder::new(64));
        let after: i64 = lock(&c)
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0)).unwrap();
        assert_eq!(before, after, "an operator message must NOT be stored as a fact");
        // And it must not be quarantined either — that would be the silent-loss failure.
        assert!(!d.join("1.bad").exists(), "operator_say must be handled, not quarantined");
    }

}
