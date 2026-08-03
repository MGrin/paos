//! Session lifecycle and presence.
//!
//! **The flock disappears here.** In Python, "am I reachable?" was answered by probing
//! an advisory lock file, cross-checked against a process table that returns *nothing*
//! inside the Claude Code sandbox — so `0 orphans` meant "cannot see", not "none", and
//! four sessions in one night each hit a different false-PASS. The daemon holds the
//! listener registry, so it simply *knows* who is connected. Liveness stops being an
//! inference and becomes a lookup.

use rusqlite::{params, Connection};

/// A session is DEAF when it is alive and in rooms but has no listener attached.
///
/// This is the failure staleness cannot see: the Stop hook heartbeats every turn, so a
/// deaf session looks perfectly fresh while every message addressed to it is ignored.
/// The threshold sits above the stale window so an ordinary long turn — during which a
/// session legitimately holds no listener — cannot false-flag.
pub const DEAF_AFTER_SECS: i64 = 2400;

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub name: String,
    pub status: Option<String>,
    pub session_id: Option<String>,
    pub last_seen: Option<String>,
    pub listening: bool,
}

/// The handle vocabulary. Same words the fleet already reads, so a Rust-minted handle is
/// indistinguishable from a Python-minted one during the cutover.
const HANDLE_ADJ: [&str; 28] = [
    "swift", "brave", "calm", "clever", "bold", "quiet", "sunny", "amber", "lucky",
    "nimble", "mellow", "cosmic", "golden", "silver", "rustic", "vivid", "witty",
    "zesty", "jolly", "frosty", "stellar", "quirky", "plucky", "snappy", "dapper",
    "breezy", "spry", "keen",
];
const HANDLE_NOUN: [&str; 27] = [
    "otter", "falcon", "lynx", "heron", "bison", "marten", "tapir", "gecko", "raven",
    "cobra", "panda", "wombat", "lemur", "mantis", "narwhal", "ferret", "badger",
    "osprey", "civet", "dingo", "koala", "puffin", "quokka", "meerkat", "viper",
    "walrus", "shrike",
];

/// An unused `adjective-animal` handle, collision-checked against `sessions.name`.
///
/// `seed` drives the choice, so this is deterministic for a given seed — tests need no
/// RNG, and seeding from the session id means two sessions starting in the same instant
/// pick different names rather than racing for one.
pub fn mint_handle(conn: &Connection, seed: u64) -> rusqlite::Result<String> {
    let taken: std::collections::HashSet<String> = {
        let mut st = conn.prepare("SELECT name FROM sessions")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        rows.filter_map(Result::ok).collect()
    };
    // A cheap LCG rather than a dependency: this needs to be spread out, not secure.
    let mut x = seed | 1;
    let mut next = || { x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407); (x >> 33) as usize };
    let mut base = String::new();
    for _ in 0..200 {
        let h = format!("{}-{}", HANDLE_ADJ[next() % HANDLE_ADJ.len()],
                                 HANDLE_NOUN[next() % HANDLE_NOUN.len()]);
        if !taken.contains(&h) {
            return Ok(h);
        }
        base = h;
    }
    // Pathological exhaustion: suffix until free.
    let mut n = 2;
    while taken.contains(&format!("{base}-{n}")) {
        n += 1;
    }
    Ok(format!("{base}-{n}"))
}

/// Bind a handle to a Claude `session_id` and mark it online, minting one when none is
/// supplied.
///
/// Idempotent: re-running for the same `session_id` restores the existing handle rather
/// than minting a second one. Handles are stable for a whole session — across turns,
/// compactions and worktree renames — because they key on the session id, not the path.
///
/// An EMPTY `name` means "mint me one". That is the presence hook's case — it calls
/// `paos bus session-start --session-id <id> --ppid <n>` with no handle at all — and
/// without minting here the session would be created under the empty name, which is
/// unaddressable and collides with the next one.
pub fn session_start(
    conn: &Connection,
    session_id: &str,
    name: &str,
    pid: Option<i64>,
    now: &str,
) -> rusqlite::Result<String> {
    let minted;
    let name = if name.trim().is_empty() {
        // Seeded from the session id so concurrent starts diverge instead of colliding.
        let seed = session_id.bytes().fold(1469598103934665603u64,
                                           |h, b| (h ^ b as u64).wrapping_mul(1099511628211));
        minted = mint_handle(conn, seed)?;
        minted.as_str()
    } else {
        name
    };
    if let Ok(existing) = conn.query_row(
        "SELECT name FROM sessions WHERE session_id = ?1",
        [session_id],
        |r| r.get::<_, String>(0),
    ) {
        conn.execute(
            "UPDATE sessions SET ended_ts = NULL, stale_since = NULL, deaf_since = NULL, \
             last_seen = ?1, pid = COALESCE(?2, pid) WHERE session_id = ?3",
            params![now, pid, session_id],
        )?;
        return Ok(existing);
    }
    // `sessions.name` is the PRIMARY KEY, so a handle still held by an ARCHIVED session
    // cannot be reused as-is. Found by running against the real database: minting
    // `swift-otter` when a retired session already owned it failed with UNIQUE
    // constraint and the session got no handle at all. Suffix until free — the same
    // `adjective-animal(-N)` shape the fleet already expects.
    for attempt in 0..64 {
        let candidate = if attempt == 0 {
            name.to_string()
        } else {
            format!("{name}-{}", attempt + 1)
        };
        let taken: bool = conn
            .query_row(
                "SELECT 1 FROM sessions WHERE name = ?1",
                [&candidate],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if taken {
            continue;
        }
        conn.execute(
            "INSERT INTO sessions(name, session_id, started_ts, updated_ts, last_seen, pid) \
             VALUES(?1, ?2, ?3, ?3, ?3, ?4)",
            params![candidate, session_id, now, pid],
        )?;
        return Ok(candidate);
    }
    Err(rusqlite::Error::InvalidParameterName(format!(
        "could not mint a free handle from base {name:?} after 64 attempts"
    )))
}

/// Advance `last_seen`. Called by the Stop hook on every assistant turn, so it must be
/// cheap and must never block the input box.
pub fn heartbeat(conn: &Connection, session_id: &str, pid: Option<i64>, now: &str)
    -> rusqlite::Result<bool>
{
    // COALESCE, so a heartbeat that carries no pid leaves the recorded one alone rather
    // than blanking it — an unknown pid reads as "cannot tell", which the reaper must
    // never treat as death.
    let n = conn.execute(
        "UPDATE sessions SET last_seen = ?1, stale_since = NULL, pid = COALESCE(?2, pid) \
         WHERE session_id = ?3 AND ended_ts IS NULL",
        params![now, pid, session_id],
    )?;
    Ok(n > 0)
}

/// Retire a session: archive it and drop its room memberships.
///
/// History is preserved — the row stays, queryable — because "gone" must not mean
/// "deleted"; you still need to see who worked on what.
pub fn session_end(conn: &Connection, session_id: &str, now: &str) -> rusqlite::Result<bool> {
    let name: Option<String> = conn
        .query_row("SELECT name FROM sessions WHERE session_id = ?1", [session_id], |r| r.get(0))
        .ok();
    let Some(name) = name else { return Ok(false) };
    conn.execute("UPDATE sessions SET ended_ts = ?1 WHERE session_id = ?2", params![now, session_id])?;
    conn.execute("DELETE FROM members WHERE name = ?1", [&name])?;
    // Tasks outlive the session that claimed them, so a departing session must let go of
    // what it was holding or the board fills with work owned by handles that no longer
    // exist. This is the ONE teardown path — the clean SessionEnd hook and the reaper
    // both arrive here — which is why the release belongs here and nowhere else.
    //
    // The task keeps its state: half-finished work is worth more than a fresh `ready`
    // task, and the next session can rescue it and read the notes to learn where the
    // previous one got to.
    //
    // Best-effort on purpose. A problem in the task tables must never stop a session
    // being archived, because an un-archived session is one the roster still calls live.
    let _ = paos_tasks::store::orphan_claims_of(conn, &name, now);
    Ok(true)
}

/// Live sessions, most recently seen first.
pub fn live_sessions(conn: &Connection, listening: &dyn Fn(&str) -> bool) -> rusqlite::Result<Vec<Session>> {
    let mut stmt = conn.prepare(
        "SELECT name, status, session_id, last_seen FROM sessions \
         WHERE ended_ts IS NULL ORDER BY last_seen DESC, name",
    )?;
    let rows = stmt.query_map([], |r| {
        let name: String = r.get(0)?;
        Ok(Session {
            listening: listening(&name),
            name,
            status: r.get(1)?,
            session_id: r.get(2)?,
            last_seen: r.get(3)?,
        })
    })?;
    rows.collect()
}

/// Set (or clear) this session's current-task status.
pub fn set_status(conn: &Connection, name: &str, status: Option<&str>, now: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE sessions SET status = ?1, updated_ts = ?2 WHERE name = ?3",
        params![status, now, name],
    )?;
    Ok(())
}

/// Join a room, recording it so a restart can restore membership.
pub fn join(conn: &Connection, room: &str, name: &str, now: &str) -> rusqlite::Result<()> {
    conn.execute("INSERT OR IGNORE INTO rooms(room, created_ts) VALUES(?1, ?2)", params![room, now])?;
    conn.execute(
        "INSERT OR IGNORE INTO members(room, name, joined_ts, last_seen) VALUES(?1, ?2, ?3, ?3)",
        // NOTE: repo is set by `join_with_repo` below. Kept out of this INSERT because
        // `OR IGNORE` would silently skip the update on a rejoin, which is exactly when a
        // session that moved worktrees needs its repo corrected.
        params![room, name, now],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO room_history(name, room, last_joined) VALUES(?1, ?2, ?3)",
        params![name, room, now],
    )?;
    Ok(())
}

/// Join, and record which repository the session is working in.
///
/// `members.repo` feeds the `repo=` column of `paos bus who`, which peers read to decide
/// whether a session is even in the right codebase to ask. Nothing had written it since
/// the Rust port — the column was populated only on rows the Python had created, so every
/// session created after the cutover showed `repo=-`.
///
/// The UPDATE is separate from the INSERT on purpose: the insert is `OR IGNORE`, so on a
/// REJOIN it does nothing — and a rejoin is precisely when a session that moved worktrees
/// needs its repo corrected. Folding repo into the insert would have fixed fresh joins and
/// silently kept a stale path forever on every other path.
pub fn join_with_repo(
    conn: &Connection,
    room: &str,
    name: &str,
    now: &str,
    repo: Option<&str>,
) -> rusqlite::Result<()> {
    join(conn, room, name, now)?;
    if let Some(r) = repo.map(str::trim).filter(|r| !r.is_empty()) {
        conn.execute(
            "UPDATE members SET repo = ?1 WHERE room = ?2 AND name = ?3",
            params![r, room, name],
        )?;
    }
    Ok(())
}

/// Leave a room **on purpose**.
///
/// Deletes the history row too. In Python `leave` removed only the membership, so the
/// auto-repair in `reachable` could not distinguish "left deliberately" from "dropped by
/// a reap" and silently rejoined you — a peer asked twice to leave a private room and
/// was put back both times without knowing.
pub fn leave(conn: &Connection, room: &str, name: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM members WHERE room = ?1 AND name = ?2", params![room, name])?;
    conn.execute("DELETE FROM room_history WHERE room = ?1 AND name = ?2", params![room, name])?;
    Ok(())
}

/// Rooms this session was in before, for restoring membership after a reap or restart.
pub fn prior_rooms(conn: &Connection, name: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT room FROM room_history WHERE name = ?1 ORDER BY room")?;
    let rows = stmt.query_map([name], |r| r.get::<_, String>(0))?;
    rows.collect()
}

/// A tasked session silent for this long is flagged stale.
pub const STALE_AFTER_SECS: i64 = 1800;

/// One supervisor pass: flag stale and DEAF sessions.
///
/// Without this the digest is blind. DEAF is the failure staleness cannot see — the
/// Stop hook heartbeats every turn, so a session whose listener died looks perfectly
/// fresh while every message addressed to it is silently ignored. The operator was the
/// detector for that three times before it was automated; leaving it unimplemented in
/// the Rust daemon quietly gave that job back to them.
///
/// `listening` is supplied by the caller (the daemon's push registry), so liveness is a
/// fact rather than a process-table guess.
/// 90 minutes without a heartbeat, for a session whose pid we cannot judge. Must stay
/// above the listener's steady re-arm window (1800s) with margin: a listening session
/// heartbeats once per window, so anything tighter would reap live sessions.
pub const REAP_THRESHOLD_S: i64 = 5400;

/// Is this pid alive? `Some(true)` / `Some(false)` are authoritative; `None` means
/// **cannot tell** and must NEVER be treated as evidence of death.
///
/// `kill(pid, 0)` and nothing else. `ps` and `pgrep` are DENIED in the agent sandbox —
/// `ps -p <pid>` fails with "operation not permitted", which reads as "no such process"
/// if you only check the exit code. That mistake already shipped once here, as
/// count_own_listeners reading pgrep's exit 3 as "no listeners" and telling healthy
/// sessions to kill their own live listener.
pub fn pid_alive(pid: Option<i64>) -> Option<bool> {
    let pid = pid.filter(|p| *p > 0)?;
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    // errno lives behind a different symbol on each platform: `__error` on macOS and the
    // BSDs, `__errno_location` on glibc. Declaring only the first one made this crate
    // link on the maintainer's Mac and fail at LINK TIME on Linux, with an undefined
    // reference to `__error` — so paos did not build on Linux at all, while the README
    // said Linux was supported. Found by building in a container, which is the only
    // place that could have found it.
    #[cfg(target_os = "linux")]
    extern "C" {
        fn __errno_location() -> *mut i32;
    }
    #[cfg(not(target_os = "linux"))]
    extern "C" {
        fn __error() -> *mut i32;
    }
    #[cfg(target_os = "linux")]
    unsafe fn errno() -> i32 { *__errno_location() }
    #[cfg(not(target_os = "linux"))]
    unsafe fn errno() -> i32 { *__error() }

    let rc = unsafe { kill(pid as i32, 0) };
    if rc == 0 {
        return Some(true);
    }
    match unsafe { errno() } {
        3 => Some(false),  // ESRCH — genuinely gone
        1 => None,         // EPERM — it exists but is not ours to signal
        _ => None,
    }
}

/// Archive sessions that are really gone, cascading their memberships and cursors.
///
/// The pid is the PRIMARY signal and the heartbeat only a backstop, because a live
/// session can legitimately be silent — under DND it arms no listener and only the
/// per-turn Stop hook heartbeats:
///
///   * pid confirmed DEAD  -> reap now (fast crash detection).
///   * pid confirmed ALIVE -> NEVER reap, however old the heartbeat. This is what lets an
///     idle-but-alive session survive.
///   * pid UNKNOWN         -> fall back to the heartbeat timeout.
///
/// Getting the tri-state wrong in the "unknown means dead" direction would archive live
/// sessions and cascade their room memberships — manufacturing fleet-wide deafness from
/// a permissions error.
/// Which sessions `reap_dead` WOULD archive. Read-only.
///
/// Shared with the reaper so the preview and the sweep cannot disagree — duplicating the
/// predicate is how a dry run starts telling you something different from the real thing.
pub fn reap_candidates(conn: &Connection, now_epoch: i64) -> rusqlite::Result<Vec<(Option<String>, String)>> {
    let mut stmt = conn.prepare(
        "SELECT session_id, name, last_seen, pid FROM sessions WHERE ended_ts IS NULL")?;
    let rows: Vec<(Option<String>, String, Option<String>, Option<i64>)> =
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .filter_map(Result::ok).collect();
    let mut out = Vec::new();
    for (sid, name, last_seen, pid) in rows {
        let alive = pid_alive(pid);
        if alive == Some(true) {
            continue; // a confirmed-alive pid protects, regardless of heartbeat age
        }
        let age = last_seen.as_deref().and_then(parse_iso_epoch)
            .map(|t| now_epoch.saturating_sub(t));
        let stale = age.map(|a| a > REAP_THRESHOLD_S).unwrap_or(true);
        if alive == Some(false) || stale {
            out.push((sid, name));
        }
    }
    Ok(out)
}

/// Which member rows `prune_members` WOULD drop. Read-only.
pub fn prune_candidates(conn: &Connection, now_epoch: i64, older_than_min: i64)
    -> rusqlite::Result<Vec<(String, String)>>
{
    let cutoff = now_epoch - older_than_min * 60;
    let mut stmt = conn.prepare("SELECT room, name, last_seen FROM members")?;
    let rows: Vec<(String, String, Option<String>)> =
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .filter_map(Result::ok).collect();
    Ok(rows.into_iter()
        .filter(|(_, _, last_seen)| {
            !last_seen.as_deref().and_then(parse_iso_epoch).map(|t| t >= cutoff).unwrap_or(false)
        })
        .map(|(room, name, _)| (room, name))
        .collect())
}

pub fn reap_dead(conn: &Connection, now_epoch: i64) -> rusqlite::Result<Vec<String>> {
    // ONE predicate, shared with the preview. Two copies would drift, and a dry run that
    // disagrees with the real sweep is worse than no dry run.
    let mut reaped = Vec::new();
    for (sid, name) in reap_candidates(conn, now_epoch)? {
        if let Some(sid) = sid {
            session_end(conn, &sid, &iso_from(now_epoch))?;
        } else {
            conn.execute("UPDATE sessions SET ended_ts=?1 WHERE name=?2",
                         rusqlite::params![iso_from(now_epoch), name])?;
            conn.execute("DELETE FROM members WHERE name=?1", [&name])?;
        }
        reaped.push(name);
    }
    Ok(reaped)
}

/// Drop member rows not seen for `older_than_min` minutes. Returns `(room, name)` pairs.
///
/// Manual only — nothing schedules this. Membership loss IS deafness here, so an
/// automatic sweep on a timer would silently unsubscribe any session whose listener is
/// briefly down, which is the failure this system keeps hitting.
pub fn prune_members(conn: &Connection, now_epoch: i64, older_than_min: i64)
    -> rusqlite::Result<Vec<(String, String)>>
{
    // Same predicate the preview uses — see `reap_dead`.
    let stale = prune_candidates(conn, now_epoch, older_than_min)?;
    for (room, name) in &stale {
        conn.execute("DELETE FROM members WHERE room=?1 AND name=?2",
                     rusqlite::params![room, name])?;
    }
    Ok(stale)
}

/// A CLOSED room is purged this many days after closing, freeing the name.
pub const ROOM_PURGE_DAYS: f64 = 14.0;
/// Member rows with no session row at all are ghosts after this long.
pub const ORPHAN_MEMBER_GRACE_H: f64 = 1.0;

/// Room GC: auto-close idle rooms, purge long-closed ones. `(closed, purged)`.
///
/// The idle budget is PER KIND, not flat: a one-task room quiet for two days is
/// finished, while a standing fleet room legitimately idles for a week, and the
/// `directory` room (lobby) never closes at all. A single flat budget was wrong for
/// both ends — it either killed standing rooms or let dead task rooms accumulate.
///
/// `lobby` is excluded outright rather than relying on its kind, because a mis-tagged
/// lobby would evict every session on this machine from the one room they all share.
/// Which rooms `prune_rooms` WOULD close and purge, read-only. `(to_close, to_purge)`.
pub fn room_gc_lists(conn: &Connection, now_epoch: i64)
    -> rusqlite::Result<(Vec<String>, Vec<String>)>
{
    let budget_for = |kind: &str| -> Option<f64> {
        paos_bus_kinds().iter().find(|(k, _)| *k == kind).and_then(|(_, b)| *b)
    };
    let mut stmt = conn.prepare(
        "SELECT r.room, r.closed_ts, r.created_ts, r.kind, \
                (SELECT MAX(m.ts) FROM messages m WHERE m.room = r.room) \
         FROM rooms r WHERE r.room != 'lobby'")?;
    let rows: Vec<(String, Option<String>, Option<String>, Option<String>, Option<String>)> =
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
            .filter_map(Result::ok).collect();

    let age_days = |ts: Option<&str>| -> Option<f64> {
        ts.and_then(parse_iso_epoch).map(|t| (now_epoch - t) as f64 / 86_400.0)
    };
    let (mut to_close, mut to_purge) = (Vec::new(), Vec::new());
    for (room, closed_ts, created_ts, kind, last_msg) in rows {
        if closed_ts.is_some() {
            if age_days(closed_ts.as_deref()).map(|a| a > ROOM_PURGE_DAYS).unwrap_or(false) {
                to_purge.push(room);
            }
            continue;
        }
        let known = kind.as_deref()
            .filter(|k| paos_bus_kinds().iter().any(|(n, _)| n == k))
            .map(str::to_string)
            .unwrap_or_else(|| infer_kind(&room));
        let Some(budget) = budget_for(&known) else { continue };
        let last = last_msg.or(created_ts);
        if age_days(last.as_deref()).map(|a| a > budget).unwrap_or(false) {
            to_close.push(room);
        }
    }
    Ok((to_close, to_purge))
}

/// Counts only, for the read-only preview.
pub fn room_gc_candidates(conn: &Connection, now_epoch: i64) -> rusqlite::Result<(usize, usize)> {
    room_gc_lists(conn, now_epoch).map(|(c, p)| (c.len(), p.len()))
}

pub fn prune_rooms(conn: &Connection, now_epoch: i64) -> rusqlite::Result<(usize, usize)> {
    // ONE decision pass, shared with the preview — see `room_gc_lists`.
    let (to_close, to_purge) = room_gc_lists(conn, now_epoch)?;
    for room in &to_close {
        // Mirrors `close`: evict members and cursors, KEEP the transcript.
        conn.execute("UPDATE rooms SET closed_ts=?1 WHERE room=?2",
                     rusqlite::params![iso_from(now_epoch), room])?;
        conn.execute("DELETE FROM members WHERE room=?1", [room])?;
        conn.execute("DELETE FROM cursors WHERE room=?1", [room])?;
    }
    for room in &to_purge {
        for t in ["messages", "cursors", "members", "rooms"] {
            conn.execute(&format!("DELETE FROM {t} WHERE room=?1"), [room])?;
        }
    }
    Ok((to_close.len(), to_purge.len()))
}

/// Member rows whose session no longer exists AT ALL — ghosts that survive `forget`,
/// the v1->v2 identity migration, and any path that removed a session without cascading.
///
/// Only rows with NO session row are removed; an ENDED session's rows are cascaded
/// elsewhere. The grace period protects a member that joined before its session row was
/// written, which would otherwise be deleted the instant it appeared.
pub fn purge_orphan_members(conn: &Connection, now_epoch: i64) -> rusqlite::Result<usize> {
    let cutoff = iso_from(now_epoch - (ORPHAN_MEMBER_GRACE_H * 3600.0) as i64);
    let n = conn.execute(
        "DELETE FROM members WHERE name NOT IN (SELECT name FROM sessions) \
         AND (last_seen IS NULL OR last_seen < ?1)", [&cutoff])?;
    conn.execute(
        "DELETE FROM cursors WHERE member NOT IN (SELECT name FROM sessions) \
         AND member NOT IN (SELECT name FROM members)", [])?;
    Ok(n)
}

/// Kinds and their idle budgets in days. Mirrors `paos_bus::readonly::ROOM_KINDS`;
/// duplicated rather than depended on because paos-bus depends on nothing here and a
/// cycle would be worse than four lines.
fn paos_bus_kinds() -> [(&'static str, Option<f64>); 4] {
    [("directory", None), ("fleet", Some(14.0)), ("program", Some(7.0)), ("task", Some(2.0))]
}

fn infer_kind(room: &str) -> String {
    if room == "lobby" { return "directory".into() }
    if room.ends_with("-fleet") || room.starts_with("fleet-") { return "fleet".into() }
    "task".into()
}

pub fn supervise(
    conn: &Connection,
    now_epoch: i64,
    listening: &dyn Fn(&str) -> bool,
) -> rusqlite::Result<(usize, usize)> {
    // DELIBERATE BEHAVIOUR CHANGE (2026-07-31, ruled by the migration orchestrator), not a
    // port: liveness comes from `sessions.last_seen`, which is what the heartbeat actually
    // advances. It used to come from MAX(members.last_seen), which conflates "left the
    // room" with "stopped responding" — and that conflation manufactured a real outage
    // today. Membership loss made a live, heartbeating session look stale; stale reads as
    // deaf; and deafness was what membership loss had already caused. Circular.
    //
    // The three concepts are now separate:
    //   STALE = not heartbeating.
    //   DEAF  = heartbeating, but nothing is listening for it.
    //   Membership belongs to NEITHER — it is only used to decide whether being unheard
    //   matters at all (a session in no rooms cannot miss room traffic).
    let mut stmt = conn.prepare(
        "SELECT s.name, s.status, s.stale_since, s.deaf_since, s.last_seen, \
                (SELECT COUNT(*) FROM members m WHERE m.name = s.name) \
         FROM sessions s WHERE s.ended_ts IS NULL",
    )?;
    let rows: Vec<(String, Option<String>, Option<String>, Option<String>, Option<String>, i64)> =
        stmt.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })?
        .filter_map(Result::ok)
        .collect();

    let (mut newly_stale, mut newly_deaf) = (0usize, 0usize);
    for (name, status, stale_since, deaf_since, last_seen, rooms) in rows {
        let age = last_seen
            .as_deref()
            .and_then(crate::parse_iso_epoch)
            .map(|t| now_epoch.saturating_sub(t));

        // STALE only applies to a session that claimed a task; an idle one is not stuck.
        let is_stale = status.as_deref().map(|s| !s.is_empty()).unwrap_or(false)
            && age.map(|a| a > STALE_AFTER_SECS).unwrap_or(true);
        match (is_stale, stale_since.is_some()) {
            (true, false) => {
                conn.execute("UPDATE sessions SET stale_since=?1 WHERE name=?2",
                             rusqlite::params![iso_from(now_epoch), name])?;
                newly_stale += 1;
            }
            (false, true) => {
                conn.execute("UPDATE sessions SET stale_since=NULL WHERE name=?1", [&name])?;
            }
            _ => {}
        }

        // DEAF: in rooms, alive, but nothing is listening for it.
        let is_deaf = rooms > 0
            && !listening(&name)
            && age.map(|a| a > DEAF_AFTER_SECS).unwrap_or(false);
        match (is_deaf, deaf_since.is_some()) {
            (true, false) => {
                conn.execute("UPDATE sessions SET deaf_since=?1 WHERE name=?2",
                             rusqlite::params![iso_from(now_epoch), name])?;
                newly_deaf += 1;
            }
            (false, true) => {
                conn.execute("UPDATE sessions SET deaf_since=NULL WHERE name=?1", [&name])?;
            }
            _ => {}
        }
    }
    Ok((newly_stale, newly_deaf))
}

fn iso_from(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let tod = epoch.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, tod / 3600, (tod % 3600) / 60, tod % 60)
}

/// Parse an ISO stamp to epoch seconds. Tolerates the `YYYY-MM-DD HH:MM:SS` form
/// SQLite's datetime() writes as well as the `T…Z` form paos writes.
pub fn parse_iso_epoch(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 { return None; }
    let num = |a: usize, z: usize| -> Option<i64> { s.get(a..z)?.parse().ok() };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) { return None; }
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86_400 + h * 3600 + mi * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        paos_store::open_in_memory().unwrap()
    }

    #[test]
    fn session_start_mints_then_reuses_the_same_handle() {
        let c = db();
        let a = session_start(&c, "sid-1", "swift-otter", Some(42), "t1").unwrap();
        let b = session_start(&c, "sid-1", "other-name", Some(42), "t2").unwrap();
        assert_eq!(a, "swift-otter");
        assert_eq!(b, "swift-otter", "a handle is stable for the whole session");
    }

    #[test]
    fn session_start_revives_an_archived_session() {
        let c = db();
        session_start(&c, "sid-1", "swift-otter", None, "t1").unwrap();
        session_end(&c, "sid-1", "t2").unwrap();
        session_start(&c, "sid-1", "swift-otter", None, "t3").unwrap();
        let ended: Option<String> = c
            .query_row("SELECT ended_ts FROM sessions WHERE session_id='sid-1'", [], |r| r.get(0))
            .unwrap();
        assert!(ended.is_none(), "restart must clear the archive stamp");
    }

    #[test]
    fn heartbeat_advances_last_seen_and_clears_stale() {
        let c = db();
        session_start(&c, "sid-1", "a", None, "t1").unwrap();
        c.execute("UPDATE sessions SET stale_since='x' WHERE session_id='sid-1'", []).unwrap();
        assert!(heartbeat(&c, "sid-1", None, "t9").unwrap());
        let (seen, stale): (String, Option<String>) = c
            .query_row("SELECT last_seen, stale_since FROM sessions WHERE session_id='sid-1'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(seen, "t9");
        assert!(stale.is_none());
    }

    #[test]
    fn heartbeat_does_not_revive_an_archived_session() {
        let c = db();
        session_start(&c, "sid-1", "a", None, "t1").unwrap();
        session_end(&c, "sid-1", "t2").unwrap();
        assert!(!heartbeat(&c, "sid-1", None, "t3").unwrap());
    }

    #[test]
    fn session_end_archives_and_drops_memberships_but_keeps_history() {
        let c = db();
        session_start(&c, "sid-1", "a", None, "t1").unwrap();
        join(&c, "lobby", "a", "t1").unwrap();
        session_end(&c, "sid-1", "t2").unwrap();
        let members: i64 = c.query_row("SELECT COUNT(*) FROM members WHERE name='a'", [], |r| r.get(0)).unwrap();
        let sessions: i64 = c.query_row("SELECT COUNT(*) FROM sessions WHERE name='a'", [], |r| r.get(0)).unwrap();
        assert_eq!(members, 0, "memberships drop");
        assert_eq!(sessions, 1, "the session row is archived, not deleted");
    }

    #[test]
    fn leave_is_remembered_so_repair_does_not_rejoin_you() {
        // REGRESSION: `leave` used to delete only the membership, leaving room_history
        // intact — so the reachability repair could not tell "left on purpose" from
        // "dropped by a reap" and silently put the session back. A peer asked to leave
        // a private room twice and was rejoined both times.
        let c = db();
        join(&c, "private", "a", "t1").unwrap();
        assert_eq!(prior_rooms(&c, "a").unwrap(), vec!["private"]);
        leave(&c, "private", "a").unwrap();
        assert!(prior_rooms(&c, "a").unwrap().is_empty(), "a deliberate leave must stick");
    }

    #[test]
    fn a_reap_can_still_be_repaired() {
        // The other half: membership lost WITHOUT a leave must still be restorable.
        let c = db();
        join(&c, "lobby", "a", "t1").unwrap();
        c.execute("DELETE FROM members WHERE name='a'", []).unwrap();  // simulate a reap
        assert_eq!(prior_rooms(&c, "a").unwrap(), vec!["lobby"]);
    }

    #[test]
    fn liveness_comes_from_the_registry_not_a_lock_file() {
        // The whole flock + process-table subsystem collapses into this.
        let c = db();
        session_start(&c, "sid-1", "listening-one", None, "t1").unwrap();
        session_start(&c, "sid-2", "deaf-one", None, "t1").unwrap();
        let attached = |n: &str| n == "listening-one";
        let sessions = live_sessions(&c, &attached).unwrap();
        let by = |n: &str| sessions.iter().find(|s| s.name == n).unwrap().listening;
        assert!(by("listening-one"));
        assert!(!by("deaf-one"));
    }

    #[test]
    fn archived_sessions_are_absent_from_the_live_roster() {
        let c = db();
        session_start(&c, "sid-1", "gone", None, "t1").unwrap();
        session_end(&c, "sid-1", "t2").unwrap();
        assert!(live_sessions(&c, &|_| false).unwrap().is_empty());
    }

    #[test]
    fn status_round_trips_and_clears() {
        let c = db();
        session_start(&c, "sid-1", "a", None, "t1").unwrap();
        set_status(&c, "a", Some("building X"), "t2").unwrap();
        assert_eq!(live_sessions(&c, &|_| false).unwrap()[0].status.as_deref(), Some("building X"));
        set_status(&c, "a", None, "t3").unwrap();
        assert_eq!(live_sessions(&c, &|_| false).unwrap()[0].status, None);
    }

    #[test]
    fn join_is_idempotent() {
        let c = db();
        join(&c, "lobby", "a", "t1").unwrap();
        join(&c, "lobby", "a", "t2").unwrap();
        let n: i64 = c.query_row("SELECT COUNT(*) FROM members WHERE name='a'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn a_handle_held_by_an_archived_session_gets_a_suffix() {
        // REGRESSION, found against the live database: sessions.name is the PRIMARY KEY,
        // so minting a handle a retired session still owns failed with UNIQUE constraint
        // and the new session got no handle at all.
        let c = db();
        session_start(&c, "sid-old", "swift-otter", None, "t1").unwrap();
        session_end(&c, "sid-old", "t2").unwrap();
        let got = session_start(&c, "sid-new", "swift-otter", None, "t3").unwrap();
        assert_eq!(got, "swift-otter-2");
    }

    #[test]
    fn suffixes_keep_climbing_past_multiple_collisions() {
        let c = db();
        for (i, sid) in ["a", "b", "c"].iter().enumerate() {
            let got = session_start(&c, sid, "swift-otter", None, "t").unwrap();
            let want = if i == 0 { "swift-otter".to_string() } else { format!("swift-otter-{}", i + 1) };
            assert_eq!(got, want);
        }
    }

    #[test]
    fn an_existing_session_id_never_gets_a_second_handle() {
        // The suffix path must not fire for a session that already has a handle.
        let c = db();
        let a = session_start(&c, "sid-1", "swift-otter", None, "t1").unwrap();
        let b = session_start(&c, "sid-1", "swift-otter", None, "t2").unwrap();
        assert_eq!(a, b);
    }

    const NOW: i64 = 1_785_400_000;

    /// `seen_ago` is written to **sessions.last_seen** — the field the heartbeat advances
    /// and the only one liveness reads. It used to be written to members.last_seen, which
    /// is what made membership loss look like unresponsiveness. The member row is still
    /// created when `rooms`, because being in a room is what makes going unheard matter,
    /// but its timestamp is deliberately fresh: liveness must not depend on it.
    fn seed(c: &Connection, name: &str, status: Option<&str>, seen_ago: i64, rooms: bool) {
        c.execute("INSERT INTO sessions(name,status,updated_ts,pid,last_seen) \
                   VALUES(?1,?2,'t',?3,?4)",
                  rusqlite::params![name, status, std::process::id() as i64,
                                    iso_from(NOW - seen_ago)]).unwrap();
        if rooms {
            c.execute("INSERT INTO members(room,name,last_seen) VALUES('lobby',?1,?2)",
                      rusqlite::params![name, iso_from(NOW)]).unwrap();
        }
    }
    fn flags(c: &Connection, name: &str) -> (bool, bool) {
        c.query_row("SELECT stale_since IS NOT NULL, deaf_since IS NOT NULL \
                     FROM sessions WHERE name=?1", [name],
                    |r| Ok((r.get::<_,i64>(0)? == 1, r.get::<_,i64>(1)? == 1))).unwrap()
    }

    #[test]
    fn stale_and_deaf_have_different_thresholds() {
        // A session in the gap is stale but NOT yet deaf: an ordinary long turn
        // legitimately holds no listener, and flagging that would be noise.
        assert!(DEAF_AFTER_SECS > STALE_AFTER_SECS);
        let c = db();
        seed(&c, "gap", Some("task"), STALE_AFTER_SECS + 60, true);
        supervise(&c, NOW, &|_| false).unwrap();
        assert_eq!(flags(&c, "gap"), (true, false));
    }

    #[test]
    fn a_tasked_but_silent_session_is_flagged_stale() {
        let c = db();
        seed(&c, "busy", Some("building X"), STALE_AFTER_SECS + 60, true);
        let (stale, _) = supervise(&c, NOW, &|_| true).unwrap();
        assert_eq!(stale, 1);
        assert!(flags(&c, "busy").0);
    }

    #[test]
    fn an_idle_session_is_never_stale() {
        // No task claimed means nothing is stuck.
        let c = db();
        seed(&c, "idle", None, 99_999, true);
        supervise(&c, NOW, &|_| true).unwrap();
        assert!(!flags(&c, "idle").0);
    }

    #[test]
    fn a_session_in_rooms_with_no_listener_is_flagged_deaf() {
        // THE failure staleness cannot see: it heartbeats every turn and looks fresh
        // while every message addressed to it is ignored.
        let c = db();
        seed(&c, "deaf", None, DEAF_AFTER_SECS + 60, true);
        let (_, deaf) = supervise(&c, NOW, &|_| false).unwrap();
        assert_eq!(deaf, 1);
        assert!(flags(&c, "deaf").1);
    }

    #[test]
    fn an_attached_listener_is_never_deaf() {
        let c = db();
        seed(&c, "ok", None, DEAF_AFTER_SECS + 60, true);
        supervise(&c, NOW, &|_| true).unwrap();
        assert!(!flags(&c, "ok").1);
    }

    #[test]
    fn a_session_in_no_rooms_is_not_deaf() {
        let c = db();
        seed(&c, "solo", None, 99_999, false);
        supervise(&c, NOW, &|_| false).unwrap();
        assert!(!flags(&c, "solo").1);
    }

    #[test]
    fn flags_clear_when_the_session_recovers() {
        let c = db();
        // Past BOTH thresholds: deaf is 2400s, stale is 1800s, so an age between them
        // is stale-only. My first version of this test used that gap and failed for the
        // wrong reason.
        seed(&c, "s", Some("task"), DEAF_AFTER_SECS + 60, true);
        supervise(&c, NOW, &|_| false).unwrap();
        assert_eq!(flags(&c, "s"), (true, true));
        // Recovery is a HEARTBEAT plus an attached listener — sessions.last_seen, which is
        // what `heartbeat` writes. Touching members.last_seen used to be enough, which is
        // the conflation this change removes.
        c.execute("UPDATE sessions SET last_seen=?1 WHERE name='s'", [iso_from(NOW)]).unwrap();
        supervise(&c, NOW, &|_| true).unwrap();
        assert_eq!(flags(&c, "s"), (false, false), "recovery must clear both");
    }

    #[test]
    fn a_heartbeat_re_asserts_the_pid_every_turn() {
        // The pid is the reaper's PRIMARY confirmed-death signal, and the hook passes it
        // on every Stop. Rust accepted --ppid and silently DROPPED it, so a row could
        // carry a stale pid indefinitely — trading a fast, certain answer for a staleness
        // timeout, and failing in the silent direction. Caught by @rustic-otter-2's hook
        // parity diff; nothing in the suite would have noticed which behaviour it had,
        // which is why this asserts the WRITE and not merely the call.
        let c = db();
        session_start(&c, "sid-p", "peer", Some(111), "t").unwrap();
        let pid = |c: &Connection| -> Option<i64> {
            c.query_row("SELECT pid FROM sessions WHERE session_id='sid-p'", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(pid(&c), Some(111));

        // A new pid — the process was replaced, the session id outlived it.
        assert!(heartbeat(&c, "sid-p", Some(222), "t2").unwrap());
        assert_eq!(pid(&c), Some(222), "the heartbeat must re-assert the pid");

        // ...and a heartbeat WITHOUT one must not blank it. An unknown pid means "cannot
        // tell", which the reaper must never read as death.
        assert!(heartbeat(&c, "sid-p", None, "t3").unwrap());
        assert_eq!(pid(&c), Some(222), "a pid-less heartbeat must leave it alone");
    }

    #[test]
    fn a_heartbeat_never_revives_or_touches_an_ended_session() {
        let c = db();
        session_start(&c, "sid-e2", "gone", Some(1), "t").unwrap();
        session_end(&c, "sid-e2", "t").unwrap();
        assert!(!heartbeat(&c, "sid-e2", Some(999), "t2").unwrap(),
                "an ended session must not be heartbeat-able");
        let pid: Option<i64> = c.query_row(
            "SELECT pid FROM sessions WHERE session_id='sid-e2'", [], |r| r.get(0)).unwrap();
        assert_eq!(pid, Some(1), "and its pid must not be rewritten");
    }

    // ---- handle minting ----

    #[test]
    fn a_session_with_no_handle_is_minted_one_not_named_empty() {
        // THE HOOK'S CASE. hooks/session-presence calls session-start with a session id
        // and a ppid and NO handle. Without minting, the row is created under the empty
        // name — unaddressable, and colliding with the next session to start.
        let c = db();
        let h = session_start(&c, "sid-hook", "", Some(42), "t").unwrap();
        assert!(!h.is_empty(), "a handle must be minted");
        assert!(h.contains('-'), "adjective-animal shape: {h}");
        let stored: String = c.query_row("SELECT name FROM sessions WHERE session_id='sid-hook'",
                                         [], |r| r.get(0)).unwrap();
        assert_eq!(stored, h);
    }

    #[test]
    fn minting_never_reuses_a_live_or_archived_handle() {
        // `sessions.name` is the primary key, so a handle held by an ARCHIVED session
        // cannot be reused — minting one anyway failed the insert and the session got no
        // handle at all.
        let c = db();
        let mut seen = std::collections::HashSet::new();
        for i in 0..40 {
            let h = session_start(&c, &format!("sid-{i}"), "", None, "t").unwrap();
            assert!(seen.insert(h.clone()), "minted {h} twice");
        }
        session_end(&c, "sid-0", "t").unwrap();
        let h = session_start(&c, "sid-new", "", None, "t").unwrap();
        assert!(seen.insert(h.clone()), "an archived handle must not be reused: {h}");
    }

    #[test]
    fn minting_is_deterministic_for_a_seed_and_differs_across_seeds() {
        let c = db();
        assert_eq!(mint_handle(&c, 12345).unwrap(), mint_handle(&c, 12345).unwrap());
        assert_ne!(mint_handle(&c, 1).unwrap(), mint_handle(&c, 2).unwrap());
    }

    #[test]
    fn restarting_the_same_session_id_keeps_its_handle() {
        // The hook fires SessionStart again on resume. Minting a second handle would
        // orphan every message addressed to the first.
        let c = db();
        let first = session_start(&c, "sid-x", "", None, "t").unwrap();
        let again = session_start(&c, "sid-x", "", None, "t2").unwrap();
        assert_eq!(first, again);
    }

    #[test]
    fn an_explicit_handle_is_still_honoured() {
        let c = db();
        assert_eq!(session_start(&c, "sid-e", "chosen-name", None, "t").unwrap(),
                   "chosen-name");
    }

    // ---- room GC ----

    fn room(c: &Connection, name: &str, kind: Option<&str>, created_ago_d: f64,
            closed_ago_d: Option<f64>) {
        let d = |days: f64| iso_from(NOW - (days * 86_400.0) as i64);
        c.execute("INSERT INTO rooms(room, created_ts, kind, closed_ts) VALUES(?1,?2,?3,?4)",
                  rusqlite::params![name, d(created_ago_d), kind, closed_ago_d.map(d)]).unwrap();
    }

    #[test]
    fn lobby_is_never_auto_closed_or_purged() {
        // Evicting everyone from the one room the whole machine shares would be the
        // single most damaging thing this function could do. Excluded by NAME, not by
        // kind, so a mis-tagged lobby cannot cause it.
        let c = db();
        c.execute("INSERT INTO rooms(room, created_ts, kind) VALUES('lobby',?1,'task')",
                  [iso_from(NOW - 400 * 86_400)]).unwrap();
        assert_eq!(prune_rooms(&c, NOW).unwrap(), (0, 0));
        let open: i64 = c.query_row(
            "SELECT COUNT(*) FROM rooms WHERE room='lobby' AND closed_ts IS NULL", [],
            |r| r.get(0)).unwrap();
        assert_eq!(open, 1);
    }

    #[test]
    fn the_idle_budget_is_per_kind_not_flat() {
        // A task room quiet for 3 days is finished; a fleet room at 3 days is not. One
        // flat budget was wrong at both ends.
        let c = db();
        room(&c, "old-task", Some("task"), 3.0, None);
        room(&c, "quiet-fleet", Some("fleet"), 3.0, None);
        room(&c, "dead-fleet", Some("fleet"), 20.0, None);
        let (closed, _) = prune_rooms(&c, NOW).unwrap();
        assert_eq!(closed, 2, "the 3-day task and the 20-day fleet");
        let still_open = |r: &str| -> i64 {
            c.query_row("SELECT COUNT(*) FROM rooms WHERE room=?1 AND closed_ts IS NULL",
                        [r], |x| x.get(0)).unwrap()
        };
        assert_eq!(still_open("quiet-fleet"), 1, "a fleet room may idle for a week");
        assert_eq!(still_open("old-task"), 0);
    }

    #[test]
    fn auto_close_evicts_members_but_keeps_the_transcript() {
        let c = db();
        room(&c, "stale", Some("task"), 30.0, None);
        c.execute("INSERT INTO members(room,name) VALUES('stale','me')", []).unwrap();
        // A REAL timestamp: last activity is the newest message ts, and an unparseable
        // one reads as "age unknown", which (faithfully to the Python) leaves the room
        // open forever. Seeding 't' here made this test assert the wrong thing.
        c.execute("INSERT INTO messages(room,seq,ts,sender,target,text) \
                   VALUES('stale',1,?1,'me','@all','a decision')",
                  [iso_from(NOW - 30 * 86_400)]).unwrap();
        prune_rooms(&c, NOW).unwrap();
        let m: i64 = c.query_row("SELECT COUNT(*) FROM members WHERE room='stale'", [],
                                 |r| r.get(0)).unwrap();
        let msg: i64 = c.query_row("SELECT COUNT(*) FROM messages WHERE room='stale'", [],
                                   |r| r.get(0)).unwrap();
        assert_eq!((m, msg), (0, 1), "evicted, but the history survives");
    }

    #[test]
    fn a_long_closed_room_is_purged_and_frees_its_name() {
        let c = db();
        room(&c, "ancient", Some("task"), 60.0, Some(ROOM_PURGE_DAYS + 1.0));
        room(&c, "recent", Some("task"), 60.0, Some(1.0));
        let (_, purged) = prune_rooms(&c, NOW).unwrap();
        assert_eq!(purged, 1);
        let gone: i64 = c.query_row("SELECT COUNT(*) FROM rooms WHERE room='ancient'", [],
                                    |r| r.get(0)).unwrap();
        assert_eq!(gone, 0, "the name is freed");
        let kept: i64 = c.query_row("SELECT COUNT(*) FROM rooms WHERE room='recent'", [],
                                    |r| r.get(0)).unwrap();
        assert_eq!(kept, 1, "recently closed rooms stay readable");
    }

    #[test]
    fn a_ghost_member_is_purged_but_a_just_joined_one_is_protected() {
        // The grace period matters: a member row written a moment before its session row
        // would otherwise be deleted the instant it appeared.
        let c = db();
        c.execute("INSERT INTO members(room,name,last_seen) VALUES('lobby','ghost',?1)",
                  [iso_from(NOW - 7200)]).unwrap();
        c.execute("INSERT INTO members(room,name,last_seen) VALUES('lobby','justnow',?1)",
                  [iso_from(NOW)]).unwrap();
        assert_eq!(purge_orphan_members(&c, NOW).unwrap(), 1);
        let left: String = c.query_row("SELECT name FROM members", [], |r| r.get(0)).unwrap();
        assert_eq!(left, "justnow");
    }

    // ---- the reaper ----

    #[test]
    fn our_own_pid_is_alive_and_an_impossible_one_is_not() {
        assert_eq!(pid_alive(Some(std::process::id() as i64)), Some(true));
        // pid 1 exists but belongs to root: EPERM, which must read as UNKNOWN, not dead.
        assert_eq!(pid_alive(Some(1)), None, "EPERM is not evidence of death");
        assert_eq!(pid_alive(None), None);
        assert_eq!(pid_alive(Some(0)), None);
        assert_eq!(pid_alive(Some(-5)), None);
    }

    #[test]
    fn a_live_pid_protects_a_session_however_old_its_heartbeat() {
        // The DND case: no listener is armed, so the only heartbeat is the per-turn Stop
        // hook. A silent-but-alive session must survive, or the reaper takes it off the
        // bus and cascades its memberships.
        let c = db();
        seed(&c, "quiet", Some("task"), REAP_THRESHOLD_S + 9_999, true);
        c.execute("UPDATE sessions SET pid=?1 WHERE name='quiet'",
                  [std::process::id() as i64]).unwrap();
        assert!(reap_dead(&c, NOW).unwrap().is_empty(), "a live pid must never be reaped");
    }

    #[test]
    fn an_unknown_pid_falls_back_to_the_heartbeat_and_never_to_death() {
        let c = db();
        // pid 1 -> EPERM -> unknown. Fresh heartbeat: must survive.
        seed(&c, "fresh", None, 10, true);
        c.execute("UPDATE sessions SET pid=1 WHERE name='fresh'", []).unwrap();
        assert!(reap_dead(&c, NOW).unwrap().is_empty(),
                "unknown pid + fresh heartbeat must not be reaped");
        // Same unknown pid, but silent past the threshold: now the backstop applies.
        seed(&c, "silent", None, REAP_THRESHOLD_S + 60, true);
        c.execute("UPDATE sessions SET pid=1 WHERE name='silent'", []).unwrap();
        assert_eq!(reap_dead(&c, NOW).unwrap(), vec!["silent"]);
    }

    #[test]
    fn reaping_cascades_membership_but_keeps_the_session_row() {
        let c = db();
        seed(&c, "gone", None, REAP_THRESHOLD_S + 60, true);
        c.execute("UPDATE sessions SET pid=NULL, session_id='sid-g' WHERE name='gone'", [])
            .unwrap();
        assert_eq!(reap_dead(&c, NOW).unwrap(), vec!["gone"]);
        let members: i64 = c.query_row("SELECT COUNT(*) FROM members WHERE name='gone'", [],
                                       |r| r.get(0)).unwrap();
        assert_eq!(members, 0, "memberships are cascaded");
        let rows: i64 = c.query_row("SELECT COUNT(*) FROM sessions WHERE name='gone'", [],
                                    |r| r.get(0)).unwrap();
        assert_eq!(rows, 1, "history is archived, not deleted");
    }

    #[test]
    fn the_reap_threshold_stays_above_the_listeners_steady_window() {
        // A listening session heartbeats once per re-arm window (1800s). If the threshold
        // ever dropped to or below that, the reaper would archive live LISTENING sessions
        // and cascade their memberships — the exact deafness the listener prevents.
        assert!(REAP_THRESHOLD_S > 1800 * 2,
                "reap threshold must clear the steady window with margin");
    }

    #[test]
    fn every_preview_agrees_exactly_with_the_sweep_it_previews() {
        // The sandboxed path reports "would reap/prune/close N" from a read-only pass,
        // because "queued" cannot distinguish swept 0 from swept 400 from never ran. That
        // is only trustworthy if the preview and the sweep share one predicate — two
        // copies drift, and a dry run that disagrees with the real thing is worse than no
        // dry run. These assert they are the same code, not merely similar.
        let c = db();
        seed(&c, "dead", None, REAP_THRESHOLD_S + 60, true);
        // `seed` records OUR pid, which is alive and therefore protects the session — the
        // reaper's most important rule. Clear it so this one is genuinely reapable via the
        // heartbeat backstop; without this the fixture asserted agreement on zero.
        c.execute("UPDATE sessions SET pid=NULL WHERE name='dead'", []).unwrap();
        seed(&c, "alive", None, 10, true);
        c.execute("INSERT INTO members(room,name,last_seen) VALUES('lobby','ghost',?1)",
                  [iso_from(NOW - 7200)]).unwrap();
        room(&c, "stale-room", Some("task"), 30.0, None);

        let predicted_reap = reap_candidates(&c, NOW).unwrap().len();
        let predicted_prune = prune_candidates(&c, NOW, 60).unwrap().len();
        let predicted_rooms = room_gc_candidates(&c, NOW).unwrap();

        // Room GC first: reaping cascades memberships and would change the prune count.
        assert_eq!(prune_rooms(&c, NOW).unwrap(), predicted_rooms);
        let actual_prune = prune_members(&c, NOW, 60).unwrap().len();
        let actual_reap = reap_dead(&c, NOW).unwrap().len();
        assert_eq!(actual_reap, predicted_reap, "reap preview must match the reap");
        assert_eq!(actual_prune, predicted_prune, "prune preview must match the prune");
        assert!(predicted_reap >= 1, "the fixture must actually exercise something");
    }

    #[test]
    fn pruning_members_drops_only_the_ones_past_the_cutoff() {
        let c = db();
        c.execute("INSERT INTO members(room,name,last_seen) VALUES('lobby','fresh',?1)",
                  [iso_from(NOW - 60)]).unwrap();
        c.execute("INSERT INTO members(room,name,last_seen) VALUES('lobby','old',?1)",
                  [iso_from(NOW - 7200)]).unwrap();
        c.execute("INSERT INTO members(room,name,last_seen) VALUES('lobby','never',NULL)", [])
            .unwrap();
        let pruned = prune_members(&c, NOW, 60).unwrap();
        let names: Vec<&str> = pruned.iter().map(|(_, n)| n.as_str()).collect();
        assert!(names.contains(&"old") && names.contains(&"never"));
        assert!(!names.contains(&"fresh"), "a recently-seen member must survive");
    }

    #[test]
    fn losing_every_room_does_not_make_a_live_session_look_stale() {
        // THE OUTAGE THIS CHANGE EXISTS TO PREVENT. Liveness used to come from
        // MAX(members.last_seen), so a session dropped from its rooms had no member rows,
        // read as infinitely old, and was flagged STALE — which reads as deaf, which is
        // exactly what the membership loss had already caused. Circular, and it took a
        // live session off the bus today.
        let c = db();
        seed(&c, "dropped", Some("porting the bus"), 0, false);
        supervise(&c, NOW, &|_| true).unwrap();
        assert_eq!(flags(&c, "dropped"), (false, false),
                   "a heartbeating session in no rooms is neither stale nor deaf");
    }

    #[test]
    fn a_silent_session_is_stale_however_fresh_its_membership_is() {
        // The other direction: `members.last_seen` is touched by room bookkeeping, so a
        // session could look alive purely because something updated its member row. Only
        // the heartbeat counts.
        let c = db();
        seed(&c, "silent", Some("task"), STALE_AFTER_SECS + 60, true);
        c.execute("UPDATE members SET last_seen=?1 WHERE name='silent'", [iso_from(NOW)]).unwrap();
        supervise(&c, NOW, &|_| true).unwrap();
        assert!(flags(&c, "silent").0, "a fresh member row must not mask a silent session");
    }

    #[test]
    fn flagging_is_idempotent_so_the_alert_fires_once() {
        let c = db();
        seed(&c, "d", None, DEAF_AFTER_SECS + 60, true);
        assert_eq!(supervise(&c, NOW, &|_| false).unwrap().1, 1);
        assert_eq!(supervise(&c, NOW, &|_| false).unwrap().1, 0, "already flagged");
    }

    #[test]
    fn iso_round_trips_including_sqlite_datetime_form() {
        assert_eq!(parse_iso_epoch(&iso_from(NOW)), Some(NOW));
        // SQLite's datetime() writes a space, not a T — the daemon reads both.
        assert_eq!(parse_iso_epoch("2026-07-29 11:37:35"), parse_iso_epoch("2026-07-29T11:37:35Z"));
        for bad in ["", "nope", "2026-13-01T00:00:00Z"] {
            assert_eq!(parse_iso_epoch(bad), None);
        }
    }
}
#[cfg(test)]
mod repo_column_tests {
    use super::*;

    fn db() -> Connection {
        paos_store::open_in_memory().unwrap()
    }

    #[test]
    fn a_join_records_the_repo_so_the_roster_can_show_it() {
        // REGRESSION. Nothing wrote members.repo after the Rust port: the only production
        // INSERT listed (room, name, joined_ts, last_seen). Rows the Python had made still
        // carried a repo, so `paos bus who` looked fine on old sessions and showed `repo=-`
        // on every new one — which reads as "this session is nowhere" to a peer deciding
        // who to ask.
        let c = db();
        join_with_repo(&c, "lobby", "me", "t", Some("/Users/x/Dev/dotfiles")).unwrap();
        let got: Option<String> = c
            .query_row("SELECT repo FROM members WHERE room='lobby' AND name='me'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(got.as_deref(), Some("/Users/x/Dev/dotfiles"));
    }

    #[test]
    fn a_rejoin_from_a_new_worktree_corrects_the_repo() {
        // THE CASE THAT RULES OUT FOLDING repo INTO THE INSERT. The insert is OR IGNORE,
        // so on a rejoin it does nothing at all — and a rejoin is exactly when a session
        // that moved worktrees needs its repo updated. An INSERT-only fix would work on
        // fresh joins and keep a stale path forever everywhere else.
        let c = db();
        join_with_repo(&c, "lobby", "me", "t", Some("/old/path")).unwrap();
        join_with_repo(&c, "lobby", "me", "t", Some("/new/path")).unwrap();
        let got: Option<String> = c
            .query_row("SELECT repo FROM members WHERE room='lobby' AND name='me'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(got.as_deref(), Some("/new/path"), "a rejoin must correct the repo");
    }

    #[test]
    fn no_repo_does_not_erase_one_already_recorded() {
        // An older CLI's spool entry carries no repo. That must not blank a good value —
        // the entry still has to APPLY, it just has nothing to say about the repo.
        let c = db();
        join_with_repo(&c, "lobby", "me", "t", Some("/Users/x/Dev/dotfiles")).unwrap();
        join_with_repo(&c, "lobby", "me", "t", None).unwrap();
        join_with_repo(&c, "lobby", "me", "t", Some("   ")).unwrap();
        let got: Option<String> = c
            .query_row("SELECT repo FROM members WHERE room='lobby' AND name='me'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(got.as_deref(), Some("/Users/x/Dev/dotfiles"));
    }

    /// A task outlives the session holding it, so teardown has to let go — otherwise the
    /// board fills with work owned by handles that no longer exist and nobody can tell
    /// which of it is live.
    #[test]
    fn ending_a_session_releases_the_tasks_it_was_holding() {
        let c = paos_store::open_in_memory().unwrap();
        c.execute("INSERT INTO sessions(name, session_id) VALUES('swift-otter','sid-1')", [])
            .unwrap();
        c.execute(
            "INSERT INTO tasks(id,title,state,priority,scope,origin,created_by,claimed_by,\
             created_ts,updated_ts) VALUES('t-aaaaaa','x','in_progress',2,'global','session',\
             'swift-otter','swift-otter','2026-08-01','2026-08-01')", []).unwrap();

        assert!(session_end(&c, "sid-1", "2026-08-01T01:00:00Z").unwrap());

        let (claimed, orphaned, state): (Option<String>, i64, String) = c
            .query_row("SELECT claimed_by, orphaned, state FROM tasks WHERE id='t-aaaaaa'", [],
                       |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!(claimed, None, "the claim is released");
        assert_eq!(orphaned, 1);
        assert_eq!(state, "in_progress", "but the progress is kept, so it can be rescued");
    }

    /// The reaper reaches `session_end` too, so a crashed session's work is released by
    /// the same code path rather than a second one that could drift.
    #[test]
    fn the_reaper_releases_claims_through_the_same_path() {
        let c = paos_store::open_in_memory().unwrap();
        c.execute("INSERT INTO sessions(name, session_id, last_seen, pid) \
                   VALUES('brave-heron','sid-2','2020-01-01T00:00:00Z', NULL)", []).unwrap();
        c.execute(
            "INSERT INTO tasks(id,title,state,priority,scope,origin,created_by,claimed_by,\
             created_ts,updated_ts) VALUES('t-bbbbbb','y','in_progress',2,'global','session',\
             'brave-heron','brave-heron','2026-08-01','2026-08-01')", []).unwrap();

        let reaped = reap_dead(&c, 4_000_000_000).unwrap();
        assert!(reaped.contains(&"brave-heron".to_string()), "reaped: {reaped:?}");

        let claimed: Option<String> = c
            .query_row("SELECT claimed_by FROM tasks WHERE id='t-bbbbbb'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(claimed, None);
    }
}
