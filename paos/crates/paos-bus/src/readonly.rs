//! Read-only bus access — the path a **sandboxed** session must take.
//!
//! Every agent session on this machine runs in a sandbox that denies unix sockets. A bus
//! command that can only speak to `paosd` is therefore unavailable to its only real
//! users: measured on 2026-07-31, `paosctl who`, `whoami` and `reachable` all exited 69
//! ("cannot reach paosd ... Operation not permitted") from inside an agent, while the
//! Python `paos bus who` exited 0 because it reads SQLite directly.
//!
//! So reads open the database `SQLITE_OPEN_READ_ONLY` — safe under WAL alongside the
//! daemon's writes — and never write. Writes still go to the daemon or the spool; this
//! module deliberately exposes no way to mutate anything.

use std::path::{Path, PathBuf};

/// `flock(2)`, declared rather than pulled in via the `libc` crate. It is the only
/// foreign function the bus needs, and `paos` ships as one self-contained binary.
mod sys {
    extern "C" {
        pub fn flock(fd: i32, operation: i32) -> i32;
    }
    pub const LOCK_EX: i32 = 2;
    pub const LOCK_NB: i32 = 4;
    pub const LOCK_UN: i32 = 8;
}

/// Open the bus database read-only. `None` when it does not exist yet or cannot be read
/// — callers degrade rather than fail, since a missing store is a first-run state.
pub fn open_ro(path: &Path) -> Option<rusqlite::Connection> {
    rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
}

/// Filesystem-safe form of a handle, byte-identical to the Python `_fs_safe`
/// (`[^A-Za-z0-9_.-] -> '_'`, strip leading/trailing `.`, empty becomes `anon`).
///
/// This must not drift: it decides the lock *filename*, so a divergence would make the
/// Rust probe read a different file than the Python listener writes, and every session
/// would look unlistening while being perfectly reachable.
pub fn fs_safe(name: &str) -> String {
    let mapped: String = name
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' { c } else { '_' })
        .collect();
    let stripped = mapped.trim_matches('.');
    if stripped.is_empty() { "anon".to_string() } else { stripped.to_string() }
}

/// Sanitized form of a handle for LOOKUPS, byte-identical to the Python `safe_name`
/// (`[^A-Za-z0-9_./-] -> '_'`, strip leading/trailing `.` and `/`, empty becomes `anon`).
///
/// Distinct from [`fs_safe`], and the difference is deliberate: this one keeps `/`,
/// because it names a session rather than a file. Using `fs_safe` here would silently
/// rewrite any handle containing a slash and look up a session that does not exist.
pub fn safe_name(name: &str) -> String {
    let mapped: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '/' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let stripped = mapped.trim_matches(|c| c == '.' || c == '/');
    if stripped.is_empty() { "anon".to_string() } else { stripped.to_string() }
}

/// Where a handle's listener holds its advisory lock.
pub fn listen_lock_path(root: &Path, name: &str) -> PathBuf {
    root.join("listen").join(format!("{}.lock", fs_safe(name)))
}

/// Is a live listener holding `path`, and if so what pid did it record?
///
/// Returns `None` when nobody holds the lock. Probing is **non-destructive**: the file is
/// opened WITHOUT truncation. Opening with `"w"` truncates before `flock` is attempted,
/// so a failed probe by a bystander blanks the live holder's pid — the proven cause of
/// 41 zero-byte locks out of 213 fleet-wide on 2026-07-28.
///
/// This is a kernel fact, so it needs neither the socket nor the process table. Both are
/// unavailable inside the sandbox: `pgrep` there exits 3 with "Cannot get process
/// information", which a previous version read as "no listeners" and used to tell healthy
/// sessions to kill their own live listener.
pub fn listener_pid(path: &Path) -> Option<String> {
    use std::io::Read;
    use std::os::unix::io::AsRawFd;

    if !path.exists() {
        return None;
    }
    let mut f = std::fs::OpenOptions::new().read(true).write(true).open(path).ok()?;
    // SAFETY: `f` owns a valid fd for the duration of the call.
    let held = unsafe { sys::flock(f.as_raw_fd(), sys::LOCK_EX | sys::LOCK_NB) } != 0;
    if !held {
        // We took it, so nobody was listening. Release immediately; dropping `f` would
        // release it anyway, but being explicit keeps the intent readable.
        unsafe { sys::flock(f.as_raw_fd(), sys::LOCK_UN) };
        return None;
    }
    let mut pid = String::new();
    let _ = f.read_to_string(&mut pid);
    let pid = pid.trim();
    Some(if pid.is_empty() { "?".to_string() } else { pid.to_string() })
}

/// Is a live listener armed for `name`?
pub fn is_listening(root: &Path, name: &str) -> bool {
    listener_pid(&listen_lock_path(root, name)).is_some()
}

/// Resolve the handle bound to a Claude Code session id, reading only live sessions.
///
/// An archived session must not resolve: reusing a retired handle would address messages
/// to a session that can never read them.
pub fn whoami(conn: &rusqlite::Connection, session_id: &str) -> Option<String> {
    conn.query_row(
        "SELECT name FROM sessions WHERE session_id = ?1 AND ended_ts IS NULL",
        [session_id],
        |r| r.get::<_, String>(0),
    )
    .ok()
}

/// The marker a session writes into its status when it is waiting on a human.
pub const BLOCKED_MARKER: &str = "⛔";

/// One row of the `who` roster.
#[derive(Debug, Clone, PartialEq)]
pub struct RosterRow {
    pub name: String,
    pub status: Option<String>,
    pub session_id: Option<String>,
    pub started_ts: Option<String>,
    pub last_seen: Option<String>,
    pub repo: Option<String>,
    pub stale: bool,
    pub deaf: bool,
}

/// Is this status string signalling "waiting on a human"?
pub fn is_blocked(status: Option<&str>) -> bool {
    let s = status.unwrap_or("").trim();
    s.starts_with(BLOCKED_MARKER) || s.to_lowercase().starts_with("blocked:")
}

/// The repo LABEL for the roster: the basename of the path, or `-`.
pub fn fmt_repo_path(path: Option<&str>) -> String {
    let p = path.unwrap_or("").trim_end_matches('/');
    match p.rsplit('/').next() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => "-".to_string(),
    }
}

/// Seconds between `ts` (RFC3339, `Z` or `+00:00`) and `now_epoch`.
pub fn age_s(ts: Option<&str>, now_epoch: i64) -> Option<i64> {
    let t = ts?.trim();
    if t.is_empty() {
        return None;
    }
    Some(now_epoch - parse_iso_epoch(t)?)
}

/// `(liveness, whole_minutes)` — the same three-band scale the Python prints.
///
/// The bands are not cosmetic: `gone` at 10 minutes is what a peer reads before deciding
/// to escalate, so widening or narrowing them changes fleet behaviour.
pub fn presence_of(last_seen: Option<&str>, now_epoch: i64) -> (&'static str, i64) {
    match age_s(last_seen, now_epoch) {
        None => ("gone", 0),
        Some(age) => {
            let age = age.max(0);
            let mins = age / 60;
            if age < 90 {
                ("live", mins)
            } else if age < 600 {
                ("idle", mins)
            } else {
                ("gone", mins)
            }
        }
    }
}

/// Task age as `3h07m` / `12m`, or `-` when unknown.
pub fn age_str(started_ts: Option<&str>, now_epoch: i64) -> String {
    match age_s(started_ts, now_epoch) {
        None => "-".to_string(),
        Some(a) => {
            let a = a.max(0);
            let (h, m) = (a / 3600, (a % 3600) / 60);
            if h > 0 { format!("{h}h{m:02}m") } else { format!("{m}m") }
        }
    }
}

/// Parse an RFC3339-ish UTC timestamp to a unix epoch. Accepts the `Z` and `+00:00`
/// spellings the Python writes.
pub fn parse_iso_epoch(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') {
        return None;
    }
    let n = |a: usize, z: usize| -> Option<i64> { s.get(a..z)?.parse().ok() };
    let (y, mo, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    let (hh, mm, ss) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    // days_from_civil (Howard Hinnant), the inverse of the daemon's now_iso().
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// The live (or archived) roster, ordered by name — read from `sessions`, which is the
/// identity/age/liveness source of truth. `members` is joined only for a repo label, so a
/// session in no room still appears.
pub fn roster(conn: &rusqlite::Connection, archive: bool) -> rusqlite::Result<Vec<RosterRow>> {
    let repos: std::collections::HashMap<String, String> = {
        let mut st = conn.prepare("SELECT name, repo FROM members WHERE repo IS NOT NULL")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.filter_map(|r| r.ok()).collect()
    };
    let where_ = if archive { "ended_ts IS NOT NULL" } else { "ended_ts IS NULL" };
    let sql = format!(
        "SELECT name, status, session_id, started_ts, last_seen, stale_since, deaf_since \
         FROM sessions WHERE {where_} ORDER BY name"
    );
    let mut st = conn.prepare(&sql)?;
    let rows = st.query_map([], |r| {
        let name: String = r.get(0)?;
        Ok(RosterRow {
            repo: repos.get(&name).cloned(),
            name,
            status: r.get(1)?,
            session_id: r.get(2)?,
            started_ts: r.get(3)?,
            last_seen: r.get(4)?,
            stale: r.get::<_, Option<String>>(5)?.is_some(),
            deaf: r.get::<_, Option<String>>(6)?.is_some(),
        })
    })?;
    rows.collect()
}

/// Render one roster row exactly as the Python `cmd_who` does.
///
/// The columns are documented in SKILL.md ("status, repo, task age, and last-seen") and
/// peers read them to decide who to address and whether to escalate, so this is a
/// contract, not formatting.
pub fn render_roster_row(r: &RosterRow, now_epoch: i64, dnd: bool) -> String {
    let (seen, mins) = presence_of(r.last_seen.as_deref(), now_epoch);
    let mut tag = String::new();
    if is_blocked(r.status.as_deref()) {
        tag.push_str("\tBLOCKED");
    }
    if dnd {
        tag.push_str("\tdnd");
    }
    if r.stale {
        tag.push_str("\tSTALE");
    }
    if r.deaf {
        tag.push_str("\t\u{26a0} DEAF");
    }
    let sid: String = r.session_id.as_deref().unwrap_or("?").chars().take(8).collect();
    format!(
        "{}\tstatus={}\trepo={}\tage={}\tseen={}({}m)\tsid={}{}",
        r.name,
        r.status.as_deref().unwrap_or("(idle)"),
        fmt_repo_path(r.repo.as_deref()),
        age_str(r.started_ts.as_deref(), now_epoch),
        seen,
        mins,
        sid,
        tag
    )
}

/// Where a not-yet-drained cursor advance is recorded locally.
pub fn pending_cursor_path(root: &Path, room: &str, member: &str) -> PathBuf {
    root.join("cursors").join(fs_safe(room)).join(fs_safe(member))
}

/// Record a cursor advance locally, alongside spooling it for the daemon.
///
/// WHY THIS EXISTS. The listener spools its read receipt, and the daemon applies it within
/// ~5s. Between those two moments the database still holds the OLD cursor — so a session
/// that is woken, takes a short turn and re-arms sees the same message again, is woken
/// again, and re-arms again. Measured: two consecutive `bus listen` runs both delivered
/// "wake up" and both exited 0. With the daemon healthy the drain usually wins the race;
/// with it down or slow this is an unbounded wake loop costing a full turn per cycle, and
/// it is silent — every wake looks like a legitimate new message.
///
/// The file is an OVERLAY, never authoritative: reads take the MAX of it and the database,
/// so it can only ever move a cursor forward. A stale one cannot hide a newer message.
pub fn record_pending_cursor(root: &Path, room: &str, member: &str, seq: i64) {
    let p = pending_cursor_path(root, room, member);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Only ever forward: a concurrent listener that read less must not lower it.
    if read_pending_cursor(root, room, member) < seq {
        let _ = std::fs::write(&p, seq.to_string());
    }
}

/// The locally-recorded cursor, or 0.
pub fn read_pending_cursor(root: &Path, room: &str, member: &str) -> i64 {
    std::fs::read_to_string(pending_cursor_path(root, room, member))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// The cursor a reader should use: the database, or a not-yet-drained advance, whichever
/// is further along. MAX, so this can only skip what was already delivered.
pub fn effective_cursor(conn: &rusqlite::Connection, root: &Path, room: &str, member: &str) -> i64 {
    let db: i64 = conn
        .query_row("SELECT seq FROM cursors WHERE room = ?1 AND member = ?2",
                   [room, member], |r| r.get(0))
        .unwrap_or(0);
    db.max(read_pending_cursor(root, room, member))
}

/// Is do-not-disturb latched for this handle? A marker file, so it is readable from a
/// sandbox and survives a daemon restart.
pub fn dnd_active(root: &Path, name: &str) -> bool {
    root.join("dnd").join(fs_safe(name)).exists()
}

/// Room kinds, in display order, with the idle budget after which each auto-closes.
/// `None` means "never auto-closes".
///
/// Order is load-bearing: `rooms` groups by kind and prints them in this sequence, so a
/// permanent directory is not buried under a pile of dead one-task rooms.
pub const ROOM_KINDS: [(&str, Option<f64>); 4] = [
    ("directory", None),      // lobby — the permanent presence/announce room
    ("fleet", Some(14.0)),    // standing room for a repo or repo-set + its orchestrator
    ("program", Some(7.0)),   // a multi-task workstream
    ("task", Some(2.0)),      // exactly one task; dies quickly once quiet
];
pub const DEFAULT_ROOM_KIND: &str = "task";

/// Best-effort kind for a room whose `kind` column is unset or unrecognised.
pub fn infer_room_kind(room: &str) -> &'static str {
    if room == "lobby" {
        return "directory";
    }
    if room.ends_with("-fleet") || room.starts_with("fleet-") {
        return "fleet";
    }
    DEFAULT_ROOM_KIND
}

/// The stored kind if it is one we know, else inferred from the name.
pub fn room_kind(stored: Option<&str>, room: &str) -> String {
    match stored {
        Some(k) if ROOM_KINDS.iter().any(|(n, _)| *n == k) => k.to_string(),
        _ => infer_room_kind(room).to_string(),
    }
}

/// A room as `rooms` lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct RoomRow {
    pub room: String,
    pub kind: String,
    pub repos: String,
    pub msgs: i64,
    pub members: i64,
    pub last: Option<String>,
    pub closed: bool,
}

/// Every room, newest-activity first. `all` includes closed rooms.
pub fn rooms(conn: &rusqlite::Connection, all: bool) -> rusqlite::Result<Vec<RoomRow>> {
    let where_ = if all { "" } else { "WHERE r.closed_ts IS NULL" };
    let sql = format!(
        "SELECT r.room, r.closed_ts, r.kind, COALESCE(r.repos,''), \
                (SELECT COUNT(*) FROM messages m WHERE m.room=r.room), \
                (SELECT COUNT(*) FROM members me WHERE me.room=r.room), \
                (SELECT MAX(ts) FROM messages m WHERE m.room=r.room) \
         FROM rooms r {where_} ORDER BY 7 DESC NULLS LAST"
    );
    let mut st = conn.prepare(&sql)?;
    let rows = st.query_map([], |r| {
        let room: String = r.get(0)?;
        let stored: Option<String> = r.get(2)?;
        Ok(RoomRow {
            kind: room_kind(stored.as_deref(), &room),
            room,
            closed: r.get::<_, Option<String>>(1)?.is_some(),
            repos: r.get(3)?,
            msgs: r.get(4)?,
            members: r.get(5)?,
            last: r.get(6)?,
        })
    })?;
    rows.collect()
}

/// `rooms` output: grouped by kind, in `ROOM_KINDS` order, with each group's lifetime.
pub fn render_rooms(rows: &[RoomRow]) -> Vec<String> {
    if rows.is_empty() {
        return vec!["(no rooms)".to_string()];
    }
    let mut out = Vec::new();
    for (kind, budget) in ROOM_KINDS {
        let group: Vec<&RoomRow> = rows.iter().filter(|r| r.kind == kind).collect();
        if group.is_empty() {
            continue;
        }
        let life = match budget {
            None => "· never auto-closes".to_string(),
            Some(d) => format!("· auto-closes after {}d idle", fmt_g(d)),
        };
        out.push(format!("== {} ({}) {}", kind, group.len(), life));
        for r in group {
            let repos = if r.repos.is_empty() { String::new() } else { format!("  repos={}", r.repos) };
            let tag = if r.closed { "  closed" } else { "" };
            out.push(format!(
                "  {:<34} msgs={:<5} members={:<3} last={}{}{}",
                r.room, r.msgs, r.members, r.last.as_deref().unwrap_or("-"), repos, tag
            ));
        }
    }
    out
}

/// Python's `%g`: drop a trailing `.0` so 14.0 prints as `14`, not `14.0`.
fn fmt_g(v: f64) -> String {
    if v.fract() == 0.0 { format!("{}", v as i64) } else { format!("{v}") }
}

/// A room's messages, oldest first.
pub fn messages(conn: &rusqlite::Connection, room: &str) -> rusqlite::Result<Vec<crate::Message>> {
    crate::unread(conn, room, 0)
}

/// One line of `log` / a delivered message.
pub fn format_msg(room: &str, m: &crate::Message) -> String {
    let to = if m.target.trim().is_empty() { crate::ALL } else { m.target.as_str() };
    format!("[{}] {} -> {}: {}", room, m.sender, to, m.text)
}

/// A room member, as `members` lists them.
#[derive(Debug, Clone, PartialEq)]
pub struct MemberRow {
    pub name: String,
    pub last_seen: Option<String>,
    pub repo: Option<String>,
    pub version: Option<String>,
    pub status: Option<String>,
}

/// Members of `room`, by name.
pub fn members(conn: &rusqlite::Connection, room: &str) -> rusqlite::Result<Vec<MemberRow>> {
    let mut st = conn.prepare(
        "SELECT m.name, m.last_seen, m.repo, m.version, s.status \
         FROM members m LEFT JOIN sessions s ON s.name = m.name \
         WHERE m.room = ?1 ORDER BY m.name",
    )?;
    let rows = st.query_map([room], |r| {
        Ok(MemberRow {
            name: r.get(0)?,
            last_seen: r.get(1)?,
            repo: r.get(2)?,
            version: r.get(3)?,
            status: r.get(4)?,
        })
    })?;
    rows.collect()
}

/// Render one `members` line, matching the Python column-for-column.
pub fn render_member(m: &MemberRow, now_epoch: i64, dnd: bool) -> String {
    let (seen, age) = presence_of(m.last_seen.as_deref(), now_epoch);
    format!(
        "{}\tstatus={}\trepo={}\tseen={}({}m)\tv{}\tlast_seen={}{}",
        m.name,
        m.status.as_deref().filter(|s| !s.is_empty()).unwrap_or("(idle)"),
        m.repo.as_deref().unwrap_or(""),
        seen,
        age,
        m.version.as_deref().unwrap_or("?"),
        m.last_seen.as_deref().unwrap_or("-"),
        if dnd { "\tdnd" } else { "" }
    )
}

/// Per-member read cursors for a room.
pub fn room_cursors(conn: &rusqlite::Connection, room: &str)
    -> rusqlite::Result<std::collections::HashMap<String, i64>>
{
    let mut st = conn.prepare("SELECT member, seq FROM cursors WHERE room = ?1")?;
    let rows = st.query_map([room], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    rows.collect()
}

/// Read receipts: for each of the last `tail` messages, who has read it.
///
/// This is how a session learns whether a peer actually saw a handoff, so "✓ all" versus
/// "unseen" is the difference between waiting and re-sending.
pub fn render_seen(
    msgs: &[crate::Message],
    cursors: &std::collections::HashMap<String, i64>,
    member_names: &[String],
    tail: usize,
) -> Vec<String> {
    if msgs.is_empty() {
        return vec!["(no messages)".to_string()];
    }
    let start = msgs.len().saturating_sub(tail);
    msgs[start..]
        .iter()
        .map(|m| {
            let others: Vec<&String> = member_names.iter().filter(|x| *x != &m.sender).collect();
            let mut readers: Vec<&str> = others
                .iter()
                .filter(|x| cursors.get(**x).copied().unwrap_or(0) >= m.seq)
                .map(|x| x.as_str())
                .collect();
            readers.sort_unstable();
            let status = if others.is_empty() {
                "(no other members)".to_string()
            } else if readers.len() == others.len() {
                "\u{2713} all".to_string()
            } else if !readers.is_empty() {
                format!("seen: {}", readers.join(", "))
            } else {
                "unseen".to_string()
            };
            let mut text = m.text.replace('\n', " ");
            if text.chars().count() > 50 {
                text = text.chars().take(50).collect::<String>() + "\u{2026}";
            }
            format!("#{} {} -> {}: {}  [{}]", m.seq, m.sender, m.target, text, status)
        })
        .collect()
}

/// Members of a room, by name — the roster `seen` compares cursors against.
pub fn room_member_names(conn: &rusqlite::Connection, room: &str) -> rusqlite::Result<Vec<String>> {
    let mut st = conn.prepare("SELECT name FROM members WHERE room = ?1 ORDER BY name")?;
    let rows = st.query_map([room], |r| r.get::<_, String>(0))?;
    rows.collect()
}

/// This handle's task history, oldest first.
pub fn history(conn: &rusqlite::Connection, name: &str) -> rusqlite::Result<Vec<String>> {
    let mut st = conn.prepare(
        "SELECT text, started_ts, ended_ts, state FROM task_log \
         WHERE session_name = ?1 ORDER BY id",
    )?;
    let rows = st.query_map([name], |r| {
        let text: Option<String> = r.get(0)?;
        let started: Option<String> = r.get(1)?;
        let ended: Option<String> = r.get(2)?;
        let state: Option<String> = r.get(3)?;
        Ok(format!(
            "{}{}\t[{}]\t{}",
            started.unwrap_or_default(),
            match ended {
                Some(e) => format!(" \u{2192} {e}"),
                None => " \u{2192} (open)".to_string(),
            },
            state.unwrap_or_else(|| "?".into()),
            text.unwrap_or_default()
        ))
    })?;
    rows.collect()
}

/// This handle's current status line, or empty.
pub fn get_status(conn: &rusqlite::Connection, name: &str) -> String {
    conn.query_row("SELECT status FROM sessions WHERE name = ?1", [name], |r| {
        r.get::<_, Option<String>>(0)
    })
    .ok()
    .flatten()
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::AsRawFd;

    // ---- fs_safe: the lock filename must match Python exactly ----

    #[test]
    fn fs_safe_passes_through_a_normal_handle() {
        assert_eq!(fs_safe("witty-bison-2"), "witty-bison-2");
    }

    #[test]
    fn fs_safe_replaces_a_path_separator() {
        // A '/' would silently redirect the lock into a subdirectory that does not exist.
        assert_eq!(fs_safe("owner/workspace"), "owner_workspace");
    }

    #[test]
    fn safe_name_matches_the_python_and_keeps_a_slash() {
        // Python: [^A-Za-z0-9_./-] -> '_', strip leading/trailing './', empty -> anon.
        assert_eq!(safe_name("witty-bison-2"), "witty-bison-2");
        assert_eq!(safe_name("  spaced out  "), "spaced_out");
        assert_eq!(safe_name("owner/workspace"), "owner/workspace");
        assert_eq!(safe_name("./rel/path/"), "rel/path");
        assert_eq!(safe_name(""), "anon");
        assert_eq!(safe_name("   "), "anon");
        assert_eq!(safe_name("héllo"), "h_llo");
    }

    #[test]
    fn safe_name_and_fs_safe_disagree_on_slash_and_that_is_the_point() {
        // fs_safe names a FILE, so '/' must not survive; safe_name names a SESSION, so it
        // must. Using fs_safe for a lookup would silently rewrite the handle and find
        // nothing — a wrong answer rather than an error.
        assert_eq!(fs_safe("owner/workspace"), "owner_workspace");
        assert_eq!(safe_name("owner/workspace"), "owner/workspace");
    }

    #[test]
    fn fs_safe_strips_dots_and_defaults_to_anon() {
        assert_eq!(fs_safe("..hidden.."), "hidden");
        assert_eq!(fs_safe("   "), "anon");
        assert_eq!(fs_safe(""), "anon");
    }

    // ---- the flock probe ----

    fn tmpdir() -> PathBuf {
        let base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
        let d = PathBuf::from(base).join(format!("paos-ro-{}-{:?}", std::process::id(),
                                                 std::thread::current().id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn an_absent_lock_file_means_nobody_is_listening() {
        let d = tmpdir();
        assert_eq!(listener_pid(&d.join("nothing-here.lock")), None);
        assert!(!is_listening(&d, "nobody"));
    }

    #[test]
    fn an_unheld_lock_file_means_nobody_is_listening() {
        // A leftover lock from a dead listener must read as "none", not as "live" —
        // otherwise a session that crashed mid-turn looks armed forever and never re-arms.
        let d = tmpdir();
        let p = d.join("stale.lock");
        std::fs::write(&p, "99999").unwrap();
        assert_eq!(listener_pid(&p), None);
    }

    #[test]
    fn a_held_lock_reports_the_holders_pid() {
        let d = tmpdir();
        let p = d.join("held.lock");
        std::fs::write(&p, "4242").unwrap();
        let holder = std::fs::OpenOptions::new().read(true).write(true).open(&p).unwrap();
        assert_eq!(unsafe { sys::flock(holder.as_raw_fd(), sys::LOCK_EX | sys::LOCK_NB) }, 0);

        assert_eq!(listener_pid(&p), Some("4242".to_string()));

        unsafe { sys::flock(holder.as_raw_fd(), sys::LOCK_UN) };
        assert_eq!(listener_pid(&p), None, "released lock must read as none");
    }

    #[test]
    fn probing_never_blanks_the_holders_pid() {
        // REGRESSION: opening the lock with "w" truncates BEFORE flock is attempted, so a
        // bystander's failed probe blanked the live holder's recorded pid. 41 of 213 fleet
        // locks were zero-byte on 2026-07-28 because of this.
        let d = tmpdir();
        let p = d.join("probe.lock");
        std::fs::write(&p, "1234").unwrap();
        let holder = std::fs::OpenOptions::new().read(true).write(true).open(&p).unwrap();
        unsafe { sys::flock(holder.as_raw_fd(), sys::LOCK_EX | sys::LOCK_NB) };

        for _ in 0..3 {
            assert_eq!(listener_pid(&p), Some("1234".to_string()));
        }
        assert_eq!(std::fs::read_to_string(&p).unwrap().trim(), "1234",
                   "probing must not truncate the holder's pid");
        unsafe { sys::flock(holder.as_raw_fd(), sys::LOCK_UN) };
    }

    #[test]
    fn lock_path_is_under_listen_and_named_for_the_handle() {
        let p = listen_lock_path(Path::new("/r"), "witty-bison-2");
        assert_eq!(p, PathBuf::from("/r/listen/witty-bison-2.lock"));
    }

    // ---- read-only database access ----

    // ---- the roster format: a documented contract, not cosmetics ----

    /// 2026-07-31T12:00:00Z. Cross-checked against both `date -u` and Python's
    /// datetime rather than against this module's own parser, which would have been
    /// circular — the first value here was wrong by three days and the parser was right.
    const T0: i64 = 1_785_499_200;

    fn at(offset_s: i64) -> String {
        // Cheap inverse of parse_iso_epoch for fixtures: build from a known base.
        let secs = T0 - offset_s;
        let days = secs.div_euclid(86_400);
        let tod = secs.rem_euclid(86_400);
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
        format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                y, m, d, tod / 3600, (tod % 3600) / 60, tod % 60)
    }

    #[test]
    fn iso_parsing_round_trips_both_spellings() {
        assert_eq!(parse_iso_epoch("2026-07-31T12:00:00Z"), Some(T0));
        assert_eq!(parse_iso_epoch("2026-07-31T12:00:00+00:00"), Some(T0));
        assert_eq!(parse_iso_epoch("garbage"), None);
        assert_eq!(parse_iso_epoch(""), None);
    }

    #[test]
    fn presence_bands_are_live_under_90s_idle_under_10m_then_gone() {
        // Peers read `gone` to decide whether to escalate, so the boundaries are behaviour.
        assert_eq!(presence_of(Some(&at(0)), T0), ("live", 0));
        assert_eq!(presence_of(Some(&at(89)), T0), ("live", 1));
        assert_eq!(presence_of(Some(&at(90)), T0), ("idle", 1));
        assert_eq!(presence_of(Some(&at(599)), T0), ("idle", 9));
        assert_eq!(presence_of(Some(&at(600)), T0), ("gone", 10));
        assert_eq!(presence_of(None, T0), ("gone", 0));
    }

    #[test]
    fn age_is_zero_padded_on_minutes_only_when_hours_are_shown() {
        assert_eq!(age_str(Some(&at(0)), T0), "0m");
        assert_eq!(age_str(Some(&at(12 * 60)), T0), "12m");
        assert_eq!(age_str(Some(&at(3 * 3600 + 7 * 60)), T0), "3h07m");
        assert_eq!(age_str(Some(&at(71 * 3600 + 54 * 60)), T0), "71h54m");
        assert_eq!(age_str(None, T0), "-");
    }

    #[test]
    fn a_repo_label_is_the_basename_or_a_dash() {
        assert_eq!(fmt_repo_path(Some("/Users/example/Dev/dotfiles")), "dotfiles");
        assert_eq!(fmt_repo_path(Some("/Users/example/Dev/dotfiles/")), "dotfiles");
        assert_eq!(fmt_repo_path(Some("dotfiles")), "dotfiles");
        assert_eq!(fmt_repo_path(None), "-");
        assert_eq!(fmt_repo_path(Some("")), "-");
    }

    #[test]
    fn blocked_is_detected_from_either_spelling() {
        assert!(is_blocked(Some("⛔ waiting on a human")));
        assert!(is_blocked(Some("blocked: need a decision")));
        assert!(is_blocked(Some("BLOCKED: need a decision")));
        assert!(!is_blocked(Some("working on the port")));
        assert!(!is_blocked(None));
        // "unblocked" must not trip it.
        assert!(!is_blocked(Some("unblocked, proceeding")));
    }

    #[test]
    fn a_roster_row_renders_every_documented_column() {
        // SKILL.md promises `who` shows status, repo, task age and last-seen. The first
        // Rust `who` printed only status and listening, silently dropping four columns
        // peers use to decide who to address.
        let r = RosterRow {
            name: "witty-bison-2".into(),
            status: Some("porting the bus".into()),
            session_id: Some("6f8c82a2-eba6-4938".into()),
            started_ts: Some(at(3 * 3600 + 7 * 60)),
            last_seen: Some(at(30)),
            repo: Some("/Users/example/Dev/dotfiles".into()),
            stale: false,
            deaf: false,
        };
        assert_eq!(
            render_roster_row(&r, T0, false),
            "witty-bison-2\tstatus=porting the bus\trepo=dotfiles\tage=3h07m\tseen=live(0m)\tsid=6f8c82a2"
        );
    }

    #[test]
    fn roster_tags_append_in_a_fixed_order() {
        let r = RosterRow {
            name: "n".into(),
            status: Some("⛔ need a call".into()),
            session_id: None,
            started_ts: None,
            last_seen: None,
            repo: None,
            stale: true,
            deaf: true,
        };
        let line = render_roster_row(&r, T0, true);
        // 6 fixed columns precede the tags: name, status, repo, age, seen, sid.
        let tags: Vec<&str> = line.split('\t').skip(6).collect();
        assert_eq!(tags, vec!["BLOCKED", "dnd", "STALE", "⚠ DEAF"]);
        assert!(line.contains("sid=?"), "an unbound session shows ? not a panic: {line}");
    }

    #[test]
    fn the_roster_lists_live_sessions_and_the_archive_separately() {
        let d = tmpdir();
        let db = d.join("roster.db");
        {
            let c = paos_store::open(&db).unwrap();
            paos_presence::session_start(&c, "sid-a", "alive-otter", None, &at(60)).unwrap();
            paos_presence::session_start(&c, "sid-b", "gone-bison", None, &at(60)).unwrap();
            paos_presence::session_end(&c, "sid-b", &at(0)).unwrap();
        }
        let ro = open_ro(&db).unwrap();
        let live: Vec<String> = roster(&ro, false).unwrap().iter().map(|r| r.name.clone()).collect();
        let arch: Vec<String> = roster(&ro, true).unwrap().iter().map(|r| r.name.clone()).collect();
        assert_eq!(live, vec!["alive-otter"]);
        assert_eq!(arch, vec!["gone-bison"], "history is preserved, not deleted");
    }

    #[test]
    fn a_session_in_no_room_still_appears_on_the_roster() {
        // `members` is joined only for a repo label. Joining it the other way round would
        // hide any session that has not joined a room yet.
        let d = tmpdir();
        let db = d.join("noroom.db");
        {
            let c = paos_store::open(&db).unwrap();
            paos_presence::session_start(&c, "sid-x", "roomless-otter", None, &at(60)).unwrap();
        }
        let ro = open_ro(&db).unwrap();
        let rows = roster(&ro, false).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(fmt_repo_path(rows[0].repo.as_deref()), "-");
    }

    // ---- the pending-cursor overlay ----

    #[test]
    fn a_spooled_receipt_is_honoured_before_the_daemon_drains_it() {
        // MEASURED before this existed: a listener delivered "wake up", spooled its
        // receipt, and an immediate re-arm delivered the SAME message again — both runs
        // exited 0. With the daemon down that is an unbounded wake loop costing a full
        // turn per cycle, and it is silent: every wake looks like a new message.
        let d = tmpdir();
        let db = d.join("cur.db");
        {
            let mut c = paos_store::open(&db).unwrap();
            crate::post(&mut c, "lobby", "peer", "@me", "wake up", "t", false, false).unwrap();
        }
        let ro = open_ro(&db).unwrap();
        assert_eq!(effective_cursor(&ro, &d, "lobby", "me"), 0);
        record_pending_cursor(&d, "lobby", "me", 1);
        assert_eq!(effective_cursor(&ro, &d, "lobby", "me"), 1,
                   "the undrained receipt must suppress a re-delivery");
    }

    #[test]
    fn the_overlay_can_only_move_a_cursor_forward() {
        // It is an overlay, never authoritative. If a stale value could LOWER the cursor
        // it would hide messages — strictly worse than the duplicate it prevents.
        let d = tmpdir();
        let db = d.join("fwd.db");
        {
            let c = paos_store::open(&db).unwrap();
            c.execute("INSERT INTO cursors(room,member,seq) VALUES('lobby','me',9)", []).unwrap();
        }
        let ro = open_ro(&db).unwrap();
        record_pending_cursor(&d, "lobby", "me", 3);
        assert_eq!(effective_cursor(&ro, &d, "lobby", "me"), 9,
                   "a lagging overlay must never pull the cursor back");

        record_pending_cursor(&d, "lobby", "me", 12);
        assert_eq!(effective_cursor(&ro, &d, "lobby", "me"), 12);
        // ...and a second writer holding an older value cannot undo it.
        record_pending_cursor(&d, "lobby", "me", 4);
        assert_eq!(read_pending_cursor(&d, "lobby", "me"), 12);
    }

    #[test]
    fn a_room_name_with_a_separator_cannot_escape_the_cursor_directory() {
        let p = pending_cursor_path(Path::new("/r"), "a/b", "me");
        assert_eq!(p, PathBuf::from("/r/cursors/a_b/me"));
    }

    #[test]
    fn open_ro_returns_none_for_a_missing_database() {
        assert!(open_ro(Path::new("/nonexistent/paos.db")).is_none());
    }

    #[test]
    fn a_read_only_connection_cannot_write() {
        // The architecture rule in one test: the CLI never writes SQLite. If this ever
        // starts passing a write, the multi-writer races the daemon exists to remove are
        // back.
        let d = tmpdir();
        let db = d.join("ro.db");
        {
            let c = paos_store::open(&db).unwrap();
            c.execute("INSERT INTO rooms(room, created_ts) VALUES('lobby','t')", []).unwrap();
        }
        let ro = open_ro(&db).unwrap();
        assert!(ro.execute("INSERT INTO rooms(room, created_ts) VALUES('x','t')", []).is_err());
        // ...but reads work fine.
        let n: i64 = ro.query_row("SELECT COUNT(*) FROM rooms", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn whoami_resolves_a_live_session_and_ignores_an_archived_one() {
        let d = tmpdir();
        let db = d.join("who.db");
        {
            let c = paos_store::open(&db).unwrap();
            paos_presence::session_start(&c, "sid-live", "live-otter", None, "t").unwrap();
            paos_presence::session_start(&c, "sid-gone", "gone-bison", None, "t").unwrap();
            paos_presence::session_end(&c, "sid-gone", "t").unwrap();
        }
        let ro = open_ro(&db).unwrap();
        assert_eq!(whoami(&ro, "sid-live"), Some("live-otter".to_string()));
        assert_eq!(whoami(&ro, "sid-gone"), None, "a retired handle must not resolve");
        assert_eq!(whoami(&ro, "sid-never"), None);
    }
}
