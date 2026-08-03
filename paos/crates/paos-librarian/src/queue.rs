//! `memory_proposals` — the human-gated review queue.

use rusqlite::{params, Connection, OptionalExtension};

/// All parts of a split live in ONE proposal row, joined by this separator.
///
/// N separate proposals pointing at one original would delete it on the first approval
/// and leave the still-pending parts orphaned — the fact would be gone and its
/// replacements would never land.
pub const SPLIT_SEP: &str = "\n---8<---\n";

#[derive(Debug, Clone, PartialEq)]
pub struct Proposal {
    pub id: i64,
    pub kind: String,
    pub dataset: String,
    pub scope: Option<String>,
    pub text: Option<String>,
    /// Comma-joined ids for tidy / split / supersede.
    pub target_data_id: Option<String>,
    pub rationale: Option<String>,
    pub source: Option<String>,
    pub status: String,
    pub created_ts: String,
    pub resolved_ts: Option<String>,
    pub screen: Option<String>,
    pub screen_why: Option<String>,
}

const COLS: &str = "id, kind, dataset, scope, text, target_data_id, rationale, source, \
                    status, created_ts, resolved_ts, screen, screen_why";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Proposal> {
    Ok(Proposal {
        id: r.get(0)?,
        kind: r.get(1)?,
        dataset: r.get(2)?,
        scope: r.get(3)?,
        text: r.get(4)?,
        target_data_id: r.get(5)?,
        rationale: r.get(6)?,
        source: r.get(7)?,
        status: r.get(8)?,
        created_ts: r.get(9)?,
        resolved_ts: r.get(10)?,
        screen: r.get(11)?,
        screen_why: r.get(12)?,
    })
}

/// Queue a proposal, unless an identical one is already waiting.
///
/// Running `dream` twice over the same session queued the same split twice, and a review
/// queue that shows the same decision two or three times is one you stop reading — which
/// is how `curate` became worthless.
///
/// Returns the existing id when it dedupes, so the caller cannot tell the difference and
/// does not need to.
#[allow(clippy::too_many_arguments)]
pub fn add(
    conn: &Connection,
    kind: &str,
    dataset: &str,
    text: Option<&str>,
    scope: Option<&str>,
    target_data_id: Option<&str>,
    rationale: Option<&str>,
    source: Option<&str>,
    now: &str,
) -> rusqlite::Result<i64> {
    if let Some(target) = target_data_id.filter(|t| !t.is_empty()) {
        let dup: Option<i64> = conn
            .query_row(
                "SELECT id FROM memory_proposals WHERE status='pending' AND kind=?1 \
                 AND dataset=?2 AND IFNULL(target_data_id,'')=?3",
                params![kind, dataset, target],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = dup {
            return Ok(id);
        }
    }
    let screened = if crate::screen::is_screened_kind(kind) {
        crate::screen::screen_proposal(text.unwrap_or(""))
    } else {
        None
    };
    let (flag, why) = match screened {
        Some((f, w)) => (Some(f), Some(w)),
        None => (None, None),
    };
    conn.execute(
        "INSERT INTO memory_proposals(kind, dataset, scope, text, target_data_id, \
         rationale, source, status, created_ts, screen, screen_why) \
         VALUES(?1,?2,?3,?4,?5,?6,?7,'pending',?8,?9,?10)",
        params![kind, dataset, scope, text, target_data_id, rationale, source, now, flag, why],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Resolve a proposal. Guarded on `status='pending'` so a double-approve is a no-op
/// rather than a second application.
pub fn set_status(
    conn: &Connection,
    id: i64,
    status: &str,
    now: &str,
) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE memory_proposals SET status=?1, resolved_ts=?2 \
         WHERE id=?3 AND status='pending'",
        params![status, now, id],
    )?;
    Ok(n > 0)
}

pub fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<Proposal>> {
    conn.query_row(
        &format!("SELECT {COLS} FROM memory_proposals WHERE id=?1"),
        params![id],
        row,
    )
    .optional()
}

pub fn list_pending(conn: &Connection) -> rusqlite::Result<Vec<Proposal>> {
    let mut st = conn
        .prepare(&format!("SELECT {COLS} FROM memory_proposals WHERE status='pending' ORDER BY id"))?;
    let out = st.query_map([], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(out)
}

pub fn list_all(conn: &Connection, limit: usize) -> rusqlite::Result<Vec<Proposal>> {
    let mut st = conn
        .prepare(&format!("SELECT {COLS} FROM memory_proposals ORDER BY id DESC LIMIT ?1"))?;
    let out = st
        .query_map(params![limit as i64], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(out)
}

/// Whether a fact id still exists.
///
/// Deliberately does NOT filter on `superseded`: a retired fact still counts as present
/// for the resurrection guard, because approving a split of it would put its pieces back.
///
/// Fails OPEN — an unreadable store returns true — so a missing database never silently
/// converts every pending proposal into a rejection.
pub fn fact_exists(conn: &Connection, id: &str) -> bool {
    match conn.query_row("SELECT 1 FROM memories WHERE id=?1", params![id], |_| Ok(())) {
        Ok(()) => true,
        Err(rusqlite::Error::QueryReturnedNoRows) => false,
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An in-memory store with just the columns the queue touches.
    ///
    /// Takes no PAOS_ROOT and sets no environment variable: Rust runs tests as threads in
    /// ONE process, so an env mutation races every other test in the binary.
    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE memory_proposals(
               id INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT NOT NULL, dataset TEXT NOT NULL,
               scope TEXT, text TEXT, target_data_id TEXT, rationale TEXT, source TEXT,
               status TEXT NOT NULL DEFAULT 'pending', created_ts TEXT NOT NULL,
               resolved_ts TEXT, screen TEXT, screen_why TEXT);
             CREATE TABLE memories(id TEXT PRIMARY KEY, dataset TEXT, text TEXT,
               superseded TEXT);",
        )
        .unwrap();
        c
    }

    fn add_capture(c: &Connection, text: &str) -> i64 {
        add(c, "capture", "ds", Some(text), Some("project"), None, None, Some("dream"), "T")
            .unwrap()
    }

    #[test]
    fn a_proposal_round_trips() {
        let c = db();
        let id = add_capture(&c, "a durable fact");
        let p = get(&c, id).unwrap().expect("stored");
        assert_eq!(p.kind, "capture");
        assert_eq!(p.status, "pending");
        assert_eq!(p.text.as_deref(), Some("a durable fact"));
        assert_eq!(p.screen, None, "a clean fact is not flagged");
    }

    #[test]
    fn an_identical_pending_proposal_is_not_queued_twice() {
        let c = db();
        let a = add(&c, "split", "ds", Some("x"), None, Some("f1"), None, None, "T").unwrap();
        let b = add(&c, "split", "ds", Some("x"), None, Some("f1"), None, None, "T").unwrap();
        assert_eq!(a, b, "the second call returns the existing id");
        assert_eq!(list_pending(&c).unwrap().len(), 1);
    }

    #[test]
    fn different_targets_still_queue_separately() {
        let c = db();
        add(&c, "split", "ds", Some("x"), None, Some("f1"), None, None, "T").unwrap();
        add(&c, "split", "ds", Some("x"), None, Some("f2"), None, None, "T").unwrap();
        assert_eq!(list_pending(&c).unwrap().len(), 2);
    }

    #[test]
    fn a_proposal_with_no_target_is_never_deduped() {
        // Two captures with the same text are two observations, not a duplicate row —
        // Python only dedupes when target_data_id is set.
        let c = db();
        add_capture(&c, "same text");
        add_capture(&c, "same text");
        assert_eq!(list_pending(&c).unwrap().len(), 2);
    }

    #[test]
    fn screening_is_recorded_but_never_blocks() {
        let c = db();
        let id = add_capture(&c, "all tests pass");
        let p = get(&c, id).unwrap().unwrap();
        assert_eq!(p.screen.as_deref(), Some("noise"));
        assert!(p.screen_why.as_deref().unwrap().contains("task status"));
        assert_eq!(p.status, "pending", "flagged, NOT rejected");
    }

    #[test]
    fn merged_text_from_a_tidy_is_not_screened() {
        let c = db();
        let id = add(&c, "tidy", "ds", Some("all tests pass"), None, Some("f1,f2"), None,
                     None, "T").unwrap();
        assert_eq!(get(&c, id).unwrap().unwrap().screen, None);
    }

    #[test]
    fn set_status_is_guarded_on_pending() {
        let c = db();
        let id = add_capture(&c, "a fact");
        assert!(set_status(&c, id, "approved", "T2").unwrap());
        assert!(!set_status(&c, id, "rejected", "T3").unwrap(), "no second resolution");
        let p = get(&c, id).unwrap().unwrap();
        assert_eq!(p.status, "approved");
        assert_eq!(p.resolved_ts.as_deref(), Some("T2"));
    }

    #[test]
    fn fact_exists_counts_a_retired_fact_as_present() {
        let c = db();
        c.execute("INSERT INTO memories VALUES('f1','ds','t','newer-id')", []).unwrap();
        assert!(fact_exists(&c, "f1"), "superseded is retired, not absent");
        assert!(!fact_exists(&c, "nope"));
    }

    #[test]
    fn fact_exists_fails_open_when_the_store_is_unreadable() {
        // No `memories` table at all. Returning false would auto-reject every pending
        // proposal whose sources it checks.
        let c = Connection::open_in_memory().unwrap();
        assert!(fact_exists(&c, "anything"));
    }

    #[test]
    fn list_pending_excludes_resolved() {
        let c = db();
        let a = add_capture(&c, "one");
        add_capture(&c, "two");
        set_status(&c, a, "rejected", "T2").unwrap();
        let pending = list_pending(&c).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].text.as_deref(), Some("two"));
        assert_eq!(list_all(&c, 100).unwrap().len(), 2, "list_all still shows both");
    }
}
