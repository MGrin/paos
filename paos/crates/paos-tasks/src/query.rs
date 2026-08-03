//! Reads.
//!
//! Nothing here writes, so every one of these is safe against a read-only connection —
//! which is how the CLI opens the database. That is deliberate: `paos task list`,
//! `ready` and `show` must keep working inside an agent sandbox, where the daemon socket
//! is blocked and every write has to spool.

use crate::model::{State, Task};
use crate::store::{row_to_task, COLS};
use rusqlite::{params, Connection};

/// The blocked predicate, in ONE place.
///
/// A task is blocked while any dependency is neither done nor dropped. This is a query
/// and never a column: a stored flag needs something to keep it true, and the discipline
/// that would have to do so is the same one that left `bus blocked` documented and
/// unused across 1,031 sessions.
const NOT_BLOCKED: &str = "NOT EXISTS (SELECT 1 FROM task_deps d \
                           JOIN tasks p ON p.id=d.depends_on \
                           WHERE d.task_id=t.id AND p.state NOT IN ('done','dropped'))";

pub fn is_blocked(conn: &Connection, id: &str) -> Result<bool, String> {
    let sql = format!("SELECT {NOT_BLOCKED} FROM tasks t WHERE t.id=?1");
    let free: Option<i64> = conn.query_row(&sql, params![id], |r| r.get(0)).ok();
    match free {
        None => Err(format!("no such task: {id}")),
        Some(v) => Ok(v == 0),
    }
}

/// Every task id that is currently blocked.
///
/// One query for the whole board rather than one per card: the dashboard renders a few
/// hundred at a time and a correlated subquery per row is the wrong shape for that.
pub fn blocked_ids(conn: &Connection) -> Result<std::collections::HashSet<String>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT d.task_id FROM task_deps d JOIN tasks p ON p.id=d.depends_on \
             WHERE p.state NOT IN ('done','dropped')",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    rows.collect::<rusqlite::Result<_>>().map_err(|e| e.to_string())
}

#[derive(Default)]
pub struct Filter {
    pub scope: Option<String>,
    pub repo: Option<String>,
    pub state: Option<State>,
    pub mine: Option<String>,
    pub orphaned_only: bool,
}

pub fn list(conn: &Connection, f: &Filter) -> Result<Vec<Task>, String> {
    let mut sql = format!("SELECT {COLS} FROM tasks t WHERE 1=1");
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(s) = &f.scope {
        sql.push_str(" AND t.scope=?");
        args.push(Box::new(s.clone()));
    }
    if let Some(r) = &f.repo {
        sql.push_str(" AND t.repo=?");
        args.push(Box::new(r.clone()));
    }
    if let Some(s) = &f.state {
        sql.push_str(" AND t.state=?");
        args.push(Box::new(s.as_str()));
    }
    if let Some(m) = &f.mine {
        sql.push_str(" AND t.claimed_by=?");
        args.push(Box::new(m.clone()));
    }
    if f.orphaned_only {
        sql.push_str(" AND t.orphaned=1 AND t.claimed_by IS NULL");
    }
    sql.push_str(" ORDER BY t.priority ASC, t.created_ts ASC");
    run(conn, &sql, args)
}

/// Everything, for the board. Ordering is the board's problem, not the query's.
pub fn all(conn: &Connection) -> Result<Vec<Task>, String> {
    run(
        conn,
        &format!("SELECT {COLS} FROM tasks t ORDER BY t.priority ASC, t.created_ts ASC"),
        Vec::new(),
    )
}

/// Claimable work: unowned, unblocked, and not yet finished.
///
/// Broader than `state='ready'` on purpose. An unowned `in_progress` task is a **rescue**,
/// and those sort FIRST — they carry the most context, and finishing one beats starting
/// over. `review` is excluded: unowned work awaiting the operator is their queue, not a
/// session's.
pub fn ready(conn: &Connection, repo: Option<&str>) -> Result<Vec<Task>, String> {
    let mut sql = format!(
        "SELECT {COLS} FROM tasks t \
         WHERE t.claimed_by IS NULL \
           AND t.state IN ('proposed','ready','in_progress') \
           AND {NOT_BLOCKED}"
    );
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(r) = repo {
        sql.push_str(" AND t.repo=?");
        args.push(Box::new(r.to_string()));
    }
    sql.push_str(" ORDER BY (t.state='in_progress') DESC, t.priority ASC, t.created_ts ASC");
    run(conn, &sql, args)
}

/// What the dashboard badge counts: work that needs the OPERATOR.
///
/// `proposed` is excluded even though it is new — the operator wrote those themselves,
/// and a badge for your own backlog is noise. What earns a badge is a task waiting on an
/// approval only they can give, or work the fleet dropped.
pub fn needs_operator(conn: &Connection) -> Result<i64, String> {
    conn.query_row(
        "SELECT COUNT(*) FROM tasks WHERE \
           (state='review' AND origin='operator' AND close_grant=0) \
           OR (orphaned=1 AND claimed_by IS NULL AND state NOT IN ('done','dropped'))",
        [],
        |r| r.get(0),
    )
    .map_err(|e| e.to_string())
}

fn run(
    conn: &Connection,
    sql: &str,
    args: Vec<Box<dyn rusqlite::ToSql>>,
) -> Result<Vec<Task>, String> {
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let rows = stmt
        .query_map(refs.as_slice(), row_to_task)
        .map_err(|e| e.to_string())?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| e.to_string())
}

pub struct Note {
    pub ts: String,
    pub author: String,
    pub kind: String,
    pub text: String,
}

/// Oldest first.
///
/// `show` is a briefing a rescuing session reads top to bottom to learn what the dead
/// session had already done — not an audit log where the newest line matters most.
pub fn notes(conn: &Connection, id: &str) -> Result<Vec<Note>, String> {
    let mut stmt = conn
        .prepare("SELECT ts,author,kind,text FROM task_notes WHERE task_id=?1 ORDER BY id ASC")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![id], |r| {
            Ok(Note {
                ts: r.get(0)?,
                author: r.get(1)?,
                kind: r.get(2)?,
                text: r.get(3)?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| e.to_string())
}

pub fn note_counts(conn: &Connection) -> Result<std::collections::HashMap<String, i64>, String> {
    let mut stmt = conn
        .prepare("SELECT task_id, COUNT(*) FROM task_notes GROUP BY task_id")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .map_err(|e| e.to_string())?;
    rows.collect::<rusqlite::Result<_>>().map_err(|e| e.to_string())
}

/// (dep id, dep state, dep title)
pub fn deps(conn: &Connection, id: &str) -> Result<Vec<(String, State, String)>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT p.id, p.state, p.title FROM task_deps d JOIN tasks p ON p.id=d.depends_on \
             WHERE d.task_id=?1 ORDER BY p.created_ts",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![id], |r| {
            let s: String = r.get(1)?;
            Ok((
                r.get(0)?,
                State::parse(&s).unwrap_or(State::Proposed),
                r.get(2)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use crate::store::tests::{a_task, db, TempDb};
    use crate::store::*;

    const T0: &str = "2026-08-01T00:00:00Z";
    const T1: &str = "2026-08-01T00:01:00Z";
    const T2: &str = "2026-08-01T00:02:00Z";
    const T3: &str = "2026-08-01T00:03:00Z";

    #[test]
    fn a_task_is_blocked_while_a_dependency_is_open_and_free_once_it_closes() {
        let c = db();
        let dep = create(&c, &a_task("first"), T0).unwrap();
        let t = create(&c, &a_task("second"), T1).unwrap();
        dep_add(&c, &t, &dep, T2).unwrap();

        assert!(is_blocked(&c, &t).unwrap());
        set_state(&c, &dep, State::Done, &Actor::Operator, T3).unwrap();
        assert!(!is_blocked(&c, &t).unwrap());
    }

    #[test]
    fn dropping_a_dependency_also_unblocks() {
        let c = db();
        let dep = create(&c, &a_task("abandoned"), T0).unwrap();
        let t = create(&c, &a_task("waiting"), T1).unwrap();
        dep_add(&c, &t, &dep, T2).unwrap();
        set_state(&c, &dep, State::Dropped, &Actor::Operator, T3).unwrap();
        assert!(
            !is_blocked(&c, &t).unwrap(),
            "a dropped dependency blocks nothing — it is never coming back"
        );
    }

    #[test]
    fn blocked_ids_agrees_with_is_blocked() {
        let c = db();
        let dep = create(&c, &a_task("first"), T0).unwrap();
        let t = create(&c, &a_task("second"), T1).unwrap();
        dep_add(&c, &t, &dep, T2).unwrap();
        let set = blocked_ids(&c).unwrap();
        assert!(set.contains(&t));
        assert!(!set.contains(&dep));
    }

    #[test]
    fn ready_hides_blocked_work() {
        let c = db();
        let dep = create(&c, &a_task("first"), T0).unwrap();
        let t = create(&c, &a_task("second"), T1).unwrap();
        dep_add(&c, &t, &dep, T2).unwrap();
        let ids: Vec<String> = ready(&c, None).unwrap().into_iter().map(|t| t.id).collect();
        assert!(ids.contains(&dep));
        assert!(!ids.contains(&t));
    }

    #[test]
    fn ready_puts_rescues_first() {
        let tmp = TempDb::new("qready");
        let mut c = tmp.conn();
        let fresh = create(&c, &a_task("fresh"), T0).unwrap();
        let stale = create(&c, &a_task("half done"), T1).unwrap();
        claim(&mut c, &stale, "swift-otter", T2).unwrap();
        orphan_claims_of(&c, "swift-otter", T3).unwrap();

        let ids: Vec<String> = ready(&c, None).unwrap().into_iter().map(|t| t.id).collect();
        assert_eq!(
            ids.first().map(String::as_str),
            Some(stale.as_str()),
            "an unowned in-progress rescue carries the most context, so it sorts first"
        );
        assert!(ids.contains(&fresh));
    }

    #[test]
    fn ready_includes_a_voluntarily_released_task() {
        // The regression that keying ownership off `orphaned` would reintroduce: a
        // released task has orphaned=0 and state=in_progress, so it would appear in
        // neither `ready` nor the orphan view — work owned by nobody and findable by no
        // query.
        let tmp = TempDb::new("qrelease");
        let mut c = tmp.conn();
        let id = create(&c, &a_task("given back"), T0).unwrap();
        claim(&mut c, &id, "swift-otter", T1).unwrap();
        release(&c, &id, "swift-otter", T2).unwrap();
        let ids: Vec<String> = ready(&c, None).unwrap().into_iter().map(|t| t.id).collect();
        assert!(ids.contains(&id));
    }

    #[test]
    fn ready_excludes_owned_work_and_review() {
        let tmp = TempDb::new("qexcl");
        let mut c = tmp.conn();
        let owned = create(&c, &a_task("owned"), T0).unwrap();
        claim(&mut c, &owned, "swift-otter", T1).unwrap();
        let waiting = create(&c, &a_task("awaiting operator"), T1).unwrap();
        set_state(&c, &waiting, State::Review, &Actor::Session("swift-otter"), T2).unwrap();

        let ids: Vec<String> = ready(&c, None).unwrap().into_iter().map(|t| t.id).collect();
        assert!(!ids.contains(&owned));
        assert!(
            !ids.contains(&waiting),
            "review is the operator's queue, not a session's"
        );
    }

    #[test]
    fn ready_scopes_to_a_repo_when_asked() {
        let c = db();
        let mut a = a_task("here");
        a.scope = "project".into();
        a.repo = Some("dotfiles".into());
        let here = create(&c, &a, T0).unwrap();
        let mut b = a_task("elsewhere");
        b.scope = "project".into();
        b.repo = Some("other".into());
        create(&c, &b, T1).unwrap();

        let ids: Vec<String> = ready(&c, Some("dotfiles"))
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, vec![here]);
    }

    #[test]
    fn notes_come_back_oldest_first_so_show_reads_as_a_briefing() {
        let c = db();
        let id = create(&c, &a_task("x"), T0).unwrap();
        note(&c, &id, "swift-otter", "note", "first", T1).unwrap();
        note(&c, &id, "swift-otter", "note", "second", T2).unwrap();
        let got: Vec<String> = notes(&c, &id).unwrap().into_iter().map(|n| n.text).collect();
        assert_eq!(got, vec!["first", "second"]);
    }

    #[test]
    fn the_badge_ignores_proposed_and_counts_review_and_orphans() {
        let tmp = TempDb::new("badge");
        let mut c = tmp.conn();

        // proposed: the operator wrote it themselves — not a notification.
        let mut op = a_task("just an idea");
        op.origin = Origin::Operator;
        create(&c, &op, T0).unwrap();
        assert_eq!(needs_operator(&c).unwrap(), 0);

        // review on an operator task with no grant: needs their approval.
        let mut op2 = a_task("finish me");
        op2.origin = Origin::Operator;
        let r = create(&c, &op2, T1).unwrap();
        set_state(&c, &r, State::Review, &Actor::Session("swift-otter"), T2).unwrap();
        assert_eq!(needs_operator(&c).unwrap(), 1);

        // an orphan: the fleet dropped it.
        let o = create(&c, &a_task("dropped on the floor"), T2).unwrap();
        claim(&mut c, &o, "swift-otter", T2).unwrap();
        orphan_claims_of(&c, "swift-otter", T3).unwrap();
        assert_eq!(needs_operator(&c).unwrap(), 2);
    }

    #[test]
    fn a_granted_review_task_does_not_need_the_operator() {
        let c = db();
        let mut op = a_task("you finish it");
        op.origin = Origin::Operator;
        let id = create(&c, &op, T0).unwrap();
        grant_close(&c, &id, T1).unwrap();
        set_state(&c, &id, State::Review, &Actor::Session("swift-otter"), T2).unwrap();
        assert_eq!(
            needs_operator(&c).unwrap(),
            0,
            "granting close-authority is exactly what takes it off the operator's plate"
        );
    }

    #[test]
    fn filters_compose() {
        let c = db();
        let mut a = a_task("mine here");
        a.scope = "project".into();
        a.repo = Some("dotfiles".into());
        create(&c, &a, T0).unwrap();
        let mut b = a_task("mine elsewhere");
        b.scope = "project".into();
        b.repo = Some("other".into());
        create(&c, &b, T1).unwrap();

        let f = Filter {
            repo: Some("dotfiles".into()),
            state: Some(State::Ready),
            ..Default::default()
        };
        assert_eq!(list(&c, &f).unwrap().len(), 1);
    }
}
