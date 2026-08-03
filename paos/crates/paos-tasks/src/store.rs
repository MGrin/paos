//! Writes.
//!
//! Every function takes an explicit `now` rather than reading the clock, so the tests
//! are deterministic and the daemon stays the only thing deciding what time it is.

use crate::model::*;
use rusqlite::{params, Connection};

/// Who is asking. The CLI and the dashboard each build one of these; neither
/// reimplements the policy in [`may_close`].
pub enum Actor<'a> {
    Operator,
    Session(&'a str),
}

impl Actor<'_> {
    pub fn name(&self) -> &str {
        match self {
            Actor::Operator => "operator",
            Actor::Session(n) => n,
        }
    }
}

#[derive(Debug)]
pub enum ClaimOutcome {
    Claimed,
    /// The task was already held. `holder` is `None` when it is unclaimable for another
    /// reason (already done, or dropped).
    Lost { holder: Option<String> },
}

/// A stable, content-derived id.
///
/// Sequential ids would have every session contending for the same next value. FNV-1a is
/// a collision spreader, not a security boundary, so a cryptographic hash would buy
/// nothing and cost this crate its dependency-free property.
pub fn derive_id(title: &str, created_ts: &str, created_by: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in title
        .bytes()
        .chain(created_ts.bytes())
        .chain(created_by.bytes())
    {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("t-{:06x}", h & 0xff_ffff)
}

pub fn create(conn: &Connection, t: &NewTask, now: &str) -> Result<String, String> {
    if t.title.trim().is_empty() {
        return Err("a task needs a title".into());
    }
    if !(0..=3).contains(&t.priority) {
        return Err(format!("priority must be 0..3, got {}", t.priority));
    }
    if !matches!(t.scope.as_str(), "global" | "org" | "project") {
        return Err(format!("scope must be global|org|project, got {}", t.scope));
    }
    if let Some(p) = &t.parent_id {
        let parent = get(conn, p)?.ok_or_else(|| format!("no such parent task: {p}"))?;
        if parent.parent_id.is_some() {
            return Err(format!(
                "epics are one level deep: {p} already has a parent, so it cannot be one"
            ));
        }
    }
    // D1: an operator's task is an instruction and gets triaged; a session's is
    // scaffolding and goes straight to the queue.
    let state = match (t.origin, t.start_ready) {
        (Origin::Operator, false) => State::Proposed,
        _ => State::Ready,
    };
    // Retry on the rare id collision rather than letting INSERT OR REPLACE eat a task.
    for attempt in 0..64u32 {
        let seed = if attempt == 0 {
            t.created_by.clone()
        } else {
            format!("{}#{attempt}", t.created_by)
        };
        let id = derive_id(&t.title, now, &seed);
        let r = conn.execute(
            "INSERT INTO tasks(id,title,body,state,priority,scope,org,repo,parent_id,\
             origin,created_by,room,created_ts,updated_ts) \
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?13)",
            params![
                id,
                t.title,
                t.body,
                state.as_str(),
                t.priority,
                t.scope,
                t.org,
                t.repo,
                t.parent_id,
                t.origin.as_str(),
                t.created_by,
                t.room,
                now
            ],
        );
        match r {
            Ok(_) => return Ok(id),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                continue
            }
            Err(e) => return Err(format!("create failed: {e}")),
        }
    }
    Err("could not mint a unique task id".into())
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Task>, String> {
    let mut stmt = conn
        .prepare(&format!("SELECT {COLS} FROM tasks t WHERE t.id=?1"))
        .map_err(|e| e.to_string())?;
    let mut rows = stmt
        .query_map(params![id], row_to_task)
        .map_err(|e| e.to_string())?;
    match rows.next() {
        None => Ok(None),
        Some(r) => r.map(Some).map_err(|e| e.to_string()),
    }
}

/// The column list, in the order [`row_to_task`] reads them. Shared with `query.rs` so
/// the two cannot drift into reading different indices.
pub const COLS: &str = "t.id,t.title,t.body,t.state,t.priority,t.scope,t.org,t.repo,\
                        t.parent_id,t.origin,t.created_by,t.claimed_by,t.claimed_ts,\
                        t.last_owner,t.orphaned,t.close_grant,t.room,t.created_ts,\
                        t.updated_ts,t.closed_ts";

pub fn row_to_task(r: &rusqlite::Row) -> rusqlite::Result<Task> {
    let state: String = r.get(3)?;
    let origin: String = r.get(9)?;
    Ok(Task {
        id: r.get(0)?,
        title: r.get(1)?,
        body: r.get(2)?,
        state: State::parse(&state).unwrap_or(State::Proposed),
        priority: r.get(4)?,
        scope: r.get(5)?,
        org: r.get(6)?,
        repo: r.get(7)?,
        parent_id: r.get(8)?,
        origin: Origin::parse(&origin).unwrap_or(Origin::Session),
        created_by: r.get(10)?,
        claimed_by: r.get(11)?,
        claimed_ts: r.get(12)?,
        last_owner: r.get(13)?,
        orphaned: r.get::<_, i64>(14)? != 0,
        close_grant: r.get::<_, i64>(15)? != 0,
        room: r.get(16)?,
        created_ts: r.get(17)?,
        updated_ts: r.get(18)?,
        closed_ts: r.get(19)?,
    })
}

/// The single source of truth for close authority.
///
/// | origin   | close_grant | operator | session |
/// |----------|-------------|----------|---------|
/// | session  | any         | yes      | yes     |
/// | operator | 0           | yes      | NO      |
/// | operator | 1           | yes      | yes     |
///
/// The grant exists because the operator asked for it: they wanted to authorise a
/// session to rescue and finish a task they created, without being the bottleneck.
pub fn may_close(t: &Task, actor: &Actor) -> Result<(), String> {
    match actor {
        Actor::Operator => Ok(()),
        Actor::Session(_) => {
            if t.origin == Origin::Session || t.close_grant {
                Ok(())
            } else {
                Err(format!(
                    "{} was created by the operator and needs their approval — move it to \
                     review, or ask them for `paos task grant {}`",
                    t.id, t.id
                ))
            }
        }
    }
}

/// Would this transition be refused, and why?
///
/// Read-only, so the CLI can ask it over its read-only connection BEFORE spooling a
/// write. This matters because a spooled write's only answer is "spooled": without this,
/// a session closing a task it is not allowed to close is told nothing, the daemon
/// silently refuses, and the session carries on believing the task is done. Exactly the
/// hole `claim_and_confirm` exists to close, in the one other place it opens.
///
/// The daemon still enforces — this only lets the caller be told early. Both call the
/// same [`may_close`], so there is one policy, not two.
pub fn precheck_state(conn: &Connection, id: &str, to: State, actor: &Actor)
    -> Result<(), String>
{
    let t = get(conn, id)?.ok_or_else(|| format!("no such task: {id}"))?;
    if t.state == to {
        return Ok(());
    }
    if t.state == State::Dropped {
        return Err(format!("{id} was dropped; create a new task instead"));
    }
    if to == State::Done {
        may_close(&t, actor)?;
    }
    Ok(())
}

/// Would releasing this fail? Same reasoning as [`precheck_state`].
pub fn precheck_release(conn: &Connection, id: &str, session: &str) -> Result<(), String> {
    let t = get(conn, id)?.ok_or_else(|| format!("no such task: {id}"))?;
    match t.claimed_by.as_deref() {
        Some(w) if w == session => Ok(()),
        Some(w) => Err(format!("{id} is held by {w}, not you")),
        None => Err(format!("{id} is not claimed by anyone")),
    }
}

/// Would adding this dependency create a cycle? Same reasoning as [`precheck_state`].
pub fn precheck_dep(conn: &Connection, id: &str, depends_on: &str) -> Result<(), String> {
    if id == depends_on {
        return Err(format!("{id} cannot depend on itself"));
    }
    get(conn, id)?.ok_or_else(|| format!("no such task: {id}"))?;
    get(conn, depends_on)?.ok_or_else(|| format!("no such task: {depends_on}"))?;
    if reaches(conn, depends_on, id)? {
        return Err(format!(
            "cycle: {depends_on} already depends on {id}, directly or through another task"
        ));
    }
    Ok(())
}

pub fn set_state(
    conn: &Connection,
    id: &str,
    to: State,
    actor: &Actor,
    now: &str,
) -> Result<(), String> {
    let t = get(conn, id)?.ok_or_else(|| format!("no such task: {id}"))?;
    if t.state == to {
        return Ok(());
    }
    // Reopening `done` is legitimate — the operator clears the board by hand and may
    // change their mind. Reopening `dropped` is not: it was abandoned on purpose, and
    // resurrecting it would lose the reason it was dropped.
    if t.state == State::Dropped {
        return Err(format!("{id} was dropped; create a new task instead"));
    }
    if to == State::Done {
        may_close(&t, actor)?;
    }
    let closed = if to.is_terminal() { Some(now) } else { None };
    conn.execute(
        "UPDATE tasks SET state=?1, closed_ts=?2, updated_ts=?3 WHERE id=?4",
        params![to.as_str(), closed, now, id],
    )
    .map_err(|e| format!("state change failed: {e}"))?;
    note(
        conn,
        id,
        actor.name(),
        "state",
        &format!("{} → {}", t.state.as_str(), to.as_str()),
        now,
    )
}

pub fn grant_close(conn: &Connection, id: &str, now: &str) -> Result<(), String> {
    let n = conn
        .execute(
            "UPDATE tasks SET close_grant=1, updated_ts=?1 WHERE id=?2",
            params![now, id],
        )
        .map_err(|e| e.to_string())?;
    if n == 0 {
        return Err(format!("no such task: {id}"));
    }
    note(
        conn,
        id,
        "operator",
        "state",
        "close-authority granted to sessions",
        now,
    )
}

pub fn note(
    conn: &Connection,
    id: &str,
    author: &str,
    kind: &str,
    text: &str,
    now: &str,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO task_notes(task_id, ts, author, kind, text) VALUES(?1,?2,?3,?4,?5)",
        params![id, now, author, kind, text],
    )
    .map(|_| ())
    .map_err(|e| format!("note failed: {e}"))
}

/// Take ownership, atomically.
///
/// `BEGIN IMMEDIATE` takes the write lock at statement start, so under WAL two racing
/// sessions serialise rather than one discovering `SQLITE_BUSY` halfway through. The
/// `claimed_by IS NULL` guard is what actually decides the race, and `rows_affected == 0`
/// means someone else won — the caller is TOLD so, rather than handed a task it does not
/// own.
///
/// `in_progress` is in the accepted-state list on purpose: an orphan keeps its state, so
/// a claim that only accepted `ready` could never rescue one, which is the whole point of
/// keeping the state in the first place.
pub fn claim(
    conn: &mut Connection,
    id: &str,
    session: &str,
    now: &str,
) -> Result<ClaimOutcome, String> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| format!("could not begin: {e}"))?;
    let n = tx
        .execute(
            "UPDATE tasks SET claimed_by=?1, claimed_ts=?2, state='in_progress', \
             orphaned=0, updated_ts=?2 \
             WHERE id=?3 AND claimed_by IS NULL \
               AND state IN ('proposed','ready','in_progress')",
            params![session, now, id],
        )
        .map_err(|e| format!("claim failed: {e}"))?;
    if n == 0 {
        let holder: Option<String> = tx
            .query_row("SELECT claimed_by FROM tasks WHERE id=?1", params![id], |r| {
                r.get(0)
            })
            .ok()
            .flatten();
        tx.commit().map_err(|e| e.to_string())?;
        return Ok(ClaimOutcome::Lost { holder });
    }
    tx.execute(
        "INSERT INTO task_notes(task_id, ts, author, kind, text) VALUES(?1,?2,?3,'claim',?4)",
        params![id, now, session, format!("claimed by {session}")],
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| format!("claim commit failed: {e}"))?;
    Ok(ClaimOutcome::Claimed)
}

/// Give a task back without demoting it.
///
/// State is preserved for the same reason an orphan's is: half-finished work is worth
/// more than a fresh `ready` task, and demoting it makes the next session start over.
pub fn release(conn: &Connection, id: &str, session: &str, now: &str) -> Result<(), String> {
    let n = conn
        .execute(
            "UPDATE tasks SET last_owner=claimed_by, claimed_by=NULL, claimed_ts=NULL, \
             updated_ts=?1 WHERE id=?2 AND claimed_by=?3",
            params![now, id, session],
        )
        .map_err(|e| format!("release failed: {e}"))?;
    if n == 0 {
        return Err(format!("{id} is not yours to release"));
    }
    note(conn, id, session, "claim", &format!("released by {session}"), now)
}

/// Release everything a departing session was holding.
///
/// Called from `paos_presence::session_end` — the ONE place a session is torn down,
/// whether cleanly by the SessionEnd hook or by the reaper. Hooking there means the two
/// paths cannot drift apart.
pub fn orphan_claims_of(conn: &Connection, session: &str, now: &str) -> Result<usize, String> {
    let ids: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT id FROM tasks WHERE claimed_by=?1 AND state NOT IN ('done','dropped')",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![session], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        rows.filter_map(Result::ok).collect()
    };
    for id in &ids {
        conn.execute(
            "UPDATE tasks SET last_owner=claimed_by, claimed_by=NULL, claimed_ts=NULL, \
             orphaned=1, updated_ts=?1 WHERE id=?2",
            params![now, id],
        )
        .map_err(|e| format!("orphan failed: {e}"))?;
        // The first thing a rescuing session reads.
        note(
            conn,
            id,
            "paos",
            "orphan",
            &format!("{session} ended while holding this; unowned and open to rescue"),
            now,
        )?;
    }
    Ok(ids.len())
}

/// Add `id depends on other`, refusing anything that would make the graph cyclic.
///
/// A cycle would make [`crate::query::is_blocked`] mutually true forever: two tasks each
/// waiting on the other, both invisible to `ready`, with nothing on the board saying why.
pub fn dep_add(conn: &Connection, id: &str, depends_on: &str, now: &str) -> Result<(), String> {
    if id == depends_on {
        return Err(format!("{id} cannot depend on itself"));
    }
    get(conn, id)?.ok_or_else(|| format!("no such task: {id}"))?;
    get(conn, depends_on)?.ok_or_else(|| format!("no such task: {depends_on}"))?;
    if reaches(conn, depends_on, id)? {
        return Err(format!(
            "cycle: {depends_on} already depends on {id}, directly or through another task"
        ));
    }
    let n = conn
        .execute(
            "INSERT OR IGNORE INTO task_deps(task_id, depends_on) VALUES(?1,?2)",
            params![id, depends_on],
        )
        .map_err(|e| format!("dep add failed: {e}"))?;
    if n == 0 {
        return Ok(()); // already there; adding twice is not an error
    }
    note(conn, id, "paos", "note", &format!("now depends on {depends_on}"), now)
}

pub fn dep_rm(conn: &Connection, id: &str, depends_on: &str) -> Result<(), String> {
    let n = conn
        .execute(
            "DELETE FROM task_deps WHERE task_id=?1 AND depends_on=?2",
            params![id, depends_on],
        )
        .map_err(|e| e.to_string())?;
    if n == 0 {
        return Err(format!("{id} does not depend on {depends_on}"));
    }
    Ok(())
}

/// Depth-first walk of the dependency edges: is `target` reachable from `from`?
fn reaches(conn: &Connection, from: &str, target: &str) -> Result<bool, String> {
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![from.to_string()];
    let mut stmt = conn
        .prepare("SELECT depends_on FROM task_deps WHERE task_id=?1")
        .map_err(|e| e.to_string())?;
    while let Some(cur) = stack.pop() {
        if cur == target {
            return Ok(true);
        }
        if !seen.insert(cur.clone()) {
            continue;
        }
        let rows = stmt
            .query_map(params![cur], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        for r in rows {
            stack.push(r.map_err(|e| e.to_string())?);
        }
    }
    Ok(false)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn db() -> Connection {
        paos_store::open_in_memory().unwrap()
    }

    /// A file-backed database — two connections cannot share `:memory:`, and the claim
    /// race is not worth testing against a fake.
    ///
    /// Hand-rolled rather than pulling in `tempfile`: this workspace has no such
    /// dependency and `paos-cli` already hand-rolls the same thing.
    pub(crate) struct TempDb {
        pub path: std::path::PathBuf,
    }

    impl TempDb {
        pub fn new(tag: &str) -> TempDb {
            let dir = std::env::temp_dir().join(format!(
                "paos-tasks-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("paos.db");
            paos_store::open(&path).unwrap();
            TempDb { path }
        }
        pub fn conn(&self) -> Connection {
            let c = Connection::open(&self.path).unwrap();
            c.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
            c
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            if let Some(d) = self.path.parent() {
                let _ = std::fs::remove_dir_all(d);
            }
        }
    }

    pub(crate) fn a_task(title: &str) -> NewTask {
        NewTask {
            title: title.into(),
            body: None,
            scope: "global".into(),
            org: None,
            repo: None,
            parent_id: None,
            priority: 2,
            origin: Origin::Session,
            created_by: "swift-otter".into(),
            room: None,
            start_ready: false,
        }
    }

    const T0: &str = "2026-08-01T00:00:00Z";
    const T1: &str = "2026-08-01T00:01:00Z";
    const T2: &str = "2026-08-01T00:02:00Z";
    const T3: &str = "2026-08-01T00:03:00Z";

    // ---- creation -------------------------------------------------------------

    #[test]
    fn a_session_task_starts_ready_and_an_operator_task_starts_proposed() {
        let c = db();
        let s = create(&c, &a_task("session work"), T0).unwrap();
        assert_eq!(get(&c, &s).unwrap().unwrap().state, State::Ready);

        let mut op = a_task("operator work");
        op.origin = Origin::Operator;
        op.created_by = "operator".into();
        let o = create(&c, &op, T0).unwrap();
        assert_eq!(get(&c, &o).unwrap().unwrap().state, State::Proposed);
    }

    #[test]
    fn start_ready_lets_the_operator_skip_triage() {
        let c = db();
        let mut op = a_task("urgent");
        op.origin = Origin::Operator;
        op.start_ready = true;
        let id = create(&c, &op, T0).unwrap();
        assert_eq!(get(&c, &id).unwrap().unwrap().state, State::Ready);
    }

    #[test]
    fn ids_are_content_derived_so_concurrent_creates_do_not_collide() {
        let a = derive_id("same title", T0, "swift-otter");
        let b = derive_id("same title", T0, "brave-heron");
        assert_ne!(a, b);
        assert!(a.starts_with("t-") && a.len() == 8, "got {a}");
    }

    #[test]
    fn a_colliding_id_is_retried_rather_than_overwriting() {
        let c = db();
        let n = a_task("dup");
        let first = create(&c, &n, T0).unwrap();
        let second = create(&c, &n, T0).unwrap();
        assert_ne!(first, second);
        let count: i64 = c
            .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn priority_outside_0_to_3_is_rejected() {
        let c = db();
        let mut n = a_task("bad");
        n.priority = 7;
        assert!(create(&c, &n, T0).is_err());
    }

    #[test]
    fn an_empty_title_is_rejected() {
        let c = db();
        let mut n = a_task("   ");
        n.title = "   ".into();
        assert!(create(&c, &n, T0).is_err());
    }

    #[test]
    fn an_unknown_parent_is_rejected() {
        let c = db();
        let mut n = a_task("child");
        n.parent_id = Some("t-nosuch".into());
        assert!(create(&c, &n, T0).is_err());
    }

    #[test]
    fn epics_are_one_level_deep() {
        let c = db();
        let root = create(&c, &a_task("epic"), T0).unwrap();
        let mut child = a_task("child");
        child.parent_id = Some(root);
        let child = create(&c, &child, T0).unwrap();
        let mut grand = a_task("grandchild");
        grand.parent_id = Some(child);
        assert!(
            create(&c, &grand, T0).is_err(),
            "a task that already has a parent cannot itself be one"
        );
    }

    // ---- close authority ------------------------------------------------------

    #[test]
    fn a_session_may_close_its_own_scaffolding() {
        let c = db();
        let id = create(&c, &a_task("mine"), T0).unwrap();
        let t = get(&c, &id).unwrap().unwrap();
        assert!(may_close(&t, &Actor::Session("swift-otter")).is_ok());
    }

    #[test]
    fn a_session_may_not_close_an_operator_task_without_a_grant() {
        let c = db();
        let mut op = a_task("yours");
        op.origin = Origin::Operator;
        let id = create(&c, &op, T0).unwrap();
        let t = get(&c, &id).unwrap().unwrap();
        let err = may_close(&t, &Actor::Session("swift-otter")).unwrap_err();
        assert!(err.contains("grant"), "the error must name the way out, got: {err}");
        assert!(may_close(&t, &Actor::Operator).is_ok());
    }

    #[test]
    fn a_grant_lets_a_session_finish_an_operator_task() {
        let c = db();
        let mut op = a_task("rescue me");
        op.origin = Origin::Operator;
        let id = create(&c, &op, T0).unwrap();
        grant_close(&c, &id, T1).unwrap();
        let t = get(&c, &id).unwrap().unwrap();
        assert!(may_close(&t, &Actor::Session("swift-otter")).is_ok());
    }

    /// The prechecks exist because a spooled write's only answer is "spooled". If these
    /// ever stop agreeing with `set_state`/`release`/`dep_add`, a session gets exit 0 for
    /// something the daemon then refuses — which is worse than an error, because it looks
    /// like success.
    #[test]
    fn precheck_state_refuses_exactly_what_set_state_refuses() {
        let c = db();
        let mut op = a_task("operator's");
        op.origin = Origin::Operator;
        let opid = create(&c, &op, T0).unwrap();
        let mine = create(&c, &a_task("mine"), T0).unwrap();
        let session = Actor::Session("swift-otter");

        for (id, to) in [(&opid, State::Done), (&mine, State::Done), (&mine, State::Review)] {
            let pre = precheck_state(&c, id, to, &session);
            let real = set_state(&c, id, to, &session, T1);
            assert_eq!(pre.is_err(), real.is_err(),
                       "precheck and set_state disagree on {id} → {}", to.as_str());
        }
    }

    #[test]
    fn precheck_state_refuses_reviving_a_dropped_task() {
        let c = db();
        let id = create(&c, &a_task("x"), T0).unwrap();
        set_state(&c, &id, State::Dropped, &Actor::Operator, T1).unwrap();
        assert!(precheck_state(&c, &id, State::Ready, &Actor::Operator).is_err());
    }

    #[test]
    fn precheck_release_matches_release() {
        let t = TempDb::new("prerelease");
        let mut c = t.conn();
        let id = create(&c, &a_task("x"), T0).unwrap();
        assert!(precheck_release(&c, &id, "swift-otter").is_err(), "unclaimed");
        claim(&mut c, &id, "swift-otter", T1).unwrap();
        assert!(precheck_release(&c, &id, "brave-heron").is_err(), "someone else's");
        assert!(precheck_release(&c, &id, "swift-otter").is_ok());
    }

    #[test]
    fn precheck_dep_catches_the_cycle_dep_add_would_reject() {
        let c = db();
        let a = create(&c, &a_task("a"), T0).unwrap();
        let b = create(&c, &a_task("b"), T1).unwrap();
        dep_add(&c, &b, &a, T2).unwrap();
        assert!(precheck_dep(&c, &a, &b).is_err());
        assert!(dep_add(&c, &a, &b, T3).is_err());
        assert!(precheck_dep(&c, &a, "t-nosuch").is_err());
    }

    #[test]
    fn moving_to_done_goes_through_may_close() {
        let c = db();
        let mut op = a_task("yours");
        op.origin = Origin::Operator;
        let id = create(&c, &op, T0).unwrap();
        assert!(set_state(&c, &id, State::Done, &Actor::Session("swift-otter"), T1).is_err());
    }

    // ---- transitions ----------------------------------------------------------

    #[test]
    fn closing_stamps_closed_ts_and_reopening_clears_it() {
        let c = db();
        let id = create(&c, &a_task("x"), T0).unwrap();
        set_state(&c, &id, State::Done, &Actor::Session("swift-otter"), T1).unwrap();
        assert_eq!(get(&c, &id).unwrap().unwrap().closed_ts.as_deref(), Some(T1));
        set_state(&c, &id, State::Ready, &Actor::Operator, T2).unwrap();
        assert_eq!(get(&c, &id).unwrap().unwrap().closed_ts, None);
    }

    #[test]
    fn every_state_change_leaves_a_note_so_show_reads_as_a_history() {
        let c = db();
        let id = create(&c, &a_task("x"), T0).unwrap();
        set_state(&c, &id, State::Review, &Actor::Session("swift-otter"), T1).unwrap();
        let n: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM task_notes WHERE task_id=?1 AND kind='state'",
                [&id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn a_dropped_task_cannot_be_moved_on() {
        let c = db();
        let id = create(&c, &a_task("x"), T0).unwrap();
        set_state(&c, &id, State::Dropped, &Actor::Operator, T1).unwrap();
        assert!(set_state(&c, &id, State::InProgress, &Actor::Operator, T2).is_err());
    }

    // ---- claiming -------------------------------------------------------------

    #[test]
    fn exactly_one_of_two_racing_sessions_wins_the_claim() {
        let t = TempDb::new("race");
        let mut a = t.conn();
        let mut b = t.conn();
        let id = create(&a, &a_task("contested"), T0).unwrap();

        let ra = claim(&mut a, &id, "swift-otter", T1).unwrap();
        let rb = claim(&mut b, &id, "brave-heron", T1).unwrap();

        assert!(matches!(ra, ClaimOutcome::Claimed));
        match rb {
            ClaimOutcome::Lost { holder } => assert_eq!(holder.as_deref(), Some("swift-otter")),
            ClaimOutcome::Claimed => panic!("both sessions claimed the same task"),
        }
    }

    #[test]
    fn claiming_moves_a_ready_task_to_in_progress() {
        let t = TempDb::new("claim");
        let mut c = t.conn();
        let id = create(&c, &a_task("x"), T0).unwrap();
        claim(&mut c, &id, "swift-otter", T1).unwrap();
        let got = get(&c, &id).unwrap().unwrap();
        assert_eq!(got.state, State::InProgress);
        assert_eq!(got.claimed_by.as_deref(), Some("swift-otter"));
    }

    #[test]
    fn a_proposed_task_can_be_claimed_without_waiting_for_triage() {
        let t = TempDb::new("proposed");
        let mut c = t.conn();
        let mut op = a_task("operator instruction");
        op.origin = Origin::Operator;
        let id = create(&c, &op, T0).unwrap();
        assert!(matches!(
            claim(&mut c, &id, "swift-otter", T1).unwrap(),
            ClaimOutcome::Claimed
        ));
    }

    #[test]
    fn a_finished_task_cannot_be_claimed() {
        let t = TempDb::new("finished");
        let mut c = t.conn();
        let id = create(&c, &a_task("x"), T0).unwrap();
        set_state(&c, &id, State::Done, &Actor::Session("swift-otter"), T1).unwrap();
        match claim(&mut c, &id, "brave-heron", T2).unwrap() {
            ClaimOutcome::Lost { holder } => assert!(holder.is_none()),
            ClaimOutcome::Claimed => panic!("a done task must not be claimable"),
        }
    }

    // ---- orphaning and release ------------------------------------------------

    #[test]
    fn an_orphan_can_be_reclaimed_and_keeps_its_progress() {
        let t = TempDb::new("orphan");
        let mut c = t.conn();
        let id = create(&c, &a_task("half done"), T0).unwrap();
        claim(&mut c, &id, "swift-otter", T1).unwrap();
        orphan_claims_of(&c, "swift-otter", T2).unwrap();

        let got = get(&c, &id).unwrap().unwrap();
        assert_eq!(got.state, State::InProgress, "an orphan keeps its state");
        assert!(got.orphaned);
        assert_eq!(got.last_owner.as_deref(), Some("swift-otter"));

        let out = claim(&mut c, &id, "brave-heron", T3).unwrap();
        assert!(
            matches!(out, ClaimOutcome::Claimed),
            "a rescuing session must be able to take it"
        );
        let got = get(&c, &id).unwrap().unwrap();
        assert_eq!(got.state, State::InProgress);
        assert!(!got.orphaned, "claiming clears the orphan flag");
    }

    #[test]
    fn orphaning_leaves_terminal_tasks_alone() {
        let t = TempDb::new("terminal");
        let mut c = t.conn();
        let id = create(&c, &a_task("finished"), T0).unwrap();
        claim(&mut c, &id, "swift-otter", T1).unwrap();
        set_state(&c, &id, State::Done, &Actor::Session("swift-otter"), T2).unwrap();
        assert_eq!(orphan_claims_of(&c, "swift-otter", T3).unwrap(), 0);
    }

    #[test]
    fn orphaning_writes_the_note_a_rescuer_reads_first() {
        let t = TempDb::new("note");
        let mut c = t.conn();
        let id = create(&c, &a_task("x"), T0).unwrap();
        claim(&mut c, &id, "swift-otter", T1).unwrap();
        orphan_claims_of(&c, "swift-otter", T2).unwrap();
        let text: String = c
            .query_row(
                "SELECT text FROM task_notes WHERE task_id=?1 AND kind='orphan'",
                [&id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(text.contains("swift-otter"));
    }

    #[test]
    fn a_released_task_is_unowned_but_not_orphaned() {
        let t = TempDb::new("release");
        let mut c = t.conn();
        let id = create(&c, &a_task("x"), T0).unwrap();
        claim(&mut c, &id, "swift-otter", T1).unwrap();
        release(&c, &id, "swift-otter", T2).unwrap();
        let got = get(&c, &id).unwrap().unwrap();
        assert!(got.is_unowned());
        assert!(!got.orphaned, "a voluntary release is not an orphaning");
        assert_eq!(got.state, State::InProgress);
        assert_eq!(got.last_owner.as_deref(), Some("swift-otter"));
    }

    #[test]
    fn you_cannot_release_a_task_you_do_not_hold() {
        let t = TempDb::new("notyours");
        let mut c = t.conn();
        let id = create(&c, &a_task("x"), T0).unwrap();
        claim(&mut c, &id, "swift-otter", T1).unwrap();
        assert!(release(&c, &id, "brave-heron", T2).is_err());
    }

    // ---- dependencies ---------------------------------------------------------

    #[test]
    fn a_direct_cycle_is_rejected() {
        let c = db();
        let a = create(&c, &a_task("a"), T0).unwrap();
        let b = create(&c, &a_task("b"), T1).unwrap();
        dep_add(&c, &b, &a, T2).unwrap();
        let err = dep_add(&c, &a, &b, T3).unwrap_err();
        assert!(err.contains("cycle"), "got: {err}");
    }

    #[test]
    fn an_indirect_cycle_is_rejected() {
        let c = db();
        let a = create(&c, &a_task("a"), T0).unwrap();
        let b = create(&c, &a_task("b"), T1).unwrap();
        let d = create(&c, &a_task("c"), T2).unwrap();
        dep_add(&c, &b, &a, T3).unwrap();
        dep_add(&c, &d, &b, T3).unwrap();
        assert!(dep_add(&c, &a, &d, T3).is_err());
    }

    #[test]
    fn a_task_cannot_depend_on_itself() {
        let c = db();
        let a = create(&c, &a_task("a"), T0).unwrap();
        assert!(dep_add(&c, &a, &a, T1).is_err());
    }

    #[test]
    fn a_dependency_on_an_unknown_task_is_rejected() {
        let c = db();
        let a = create(&c, &a_task("a"), T0).unwrap();
        assert!(dep_add(&c, &a, "t-nosuch", T1).is_err());
    }

    #[test]
    fn adding_the_same_dependency_twice_is_not_an_error() {
        let c = db();
        let a = create(&c, &a_task("a"), T0).unwrap();
        let b = create(&c, &a_task("b"), T1).unwrap();
        dep_add(&c, &a, &b, T2).unwrap();
        dep_add(&c, &a, &b, T3).unwrap();
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM task_deps WHERE task_id=?1", [&a], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn removing_a_dependency_that_is_not_there_is_an_error() {
        let c = db();
        let a = create(&c, &a_task("a"), T0).unwrap();
        let b = create(&c, &a_task("b"), T1).unwrap();
        assert!(dep_rm(&c, &a, &b).is_err());
    }
}
