//! The **only** thing that opens `paos.db`.
//!
//! The Python implementation had five modules (`bus_facet`, `operator_facet`,
//! `events_facet`, `standup_facet`, `librarian_facet`) each running `ensure_schema()`
//! for *its own* tables against the same file. First writer wins, so the database could
//! end up with a partial schema — `standup_facet.py:278` already carried a bare
//! `except` to survive a missing `messages` table. `SCHEMA_VERSION` covered bus tables
//! only; the other four migrated by ad-hoc `PRAGMA table_info` sniffing.
//!
//! Here there is exactly one schema, one version, and one ordered migration ladder.

use rusqlite::Connection;
use std::path::{Path, PathBuf};

pub mod backup;
pub mod migrations;

/// Bumped whenever a migration is appended. `migrations::LADDER.len()` must equal this.
pub const SCHEMA_VERSION: i64 = migrations::LADDER.len() as i64;

#[derive(Debug)]
pub enum StoreError {
    Sql(rusqlite::Error),
    /// The database was written by a newer paos than this binary understands.
    FromTheFuture { found: i64, known: i64 },
    /// A snapshot could not be produced or could not be trusted.
    Backup(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sql(e) => write!(f, "sqlite: {e}"),
            StoreError::Backup(m) => write!(f, "backup: {m}"),
            StoreError::FromTheFuture { found, known } => write!(
                f,
                "database schema v{found} is newer than this binary understands (v{known}) — \
                 upgrade paos rather than letting an old binary write to it"
            ),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sql(e)
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Resolve the store root: `$PAOS_ROOT`, else `~/.paos`.
///
/// Split out from `root()` so it can be tested without setting an environment variable —
/// Rust runs tests as threads in ONE process, so an env mutation races every other test
/// in the binary.
pub fn resolve_root(
    paos_root: Option<&str>,
    home: Option<&str>,
) -> std::result::Result<PathBuf, String> {
    match paos_root {
        // SET BUT EMPTY IS AN ERROR, not "unset". The only way this happens in practice is
        // a command substitution that failed, and the caller was asking for ISOLATION
        // when they wrote it. Falling back to ~/.paos silently gives them the LIVE store
        // instead — the exact opposite of the request, and undetectable until you notice
        // test data in the real fleet.
        //
        // Not hypothetical: `PAOS_ROOT=$(mktemp -d) paos ...` is the documented way to
        // test writes, `mktemp -d` is DENIED in the agent sandbox ("Operation not
        // permitted"), and a session lost a spooled op into the live store this way — it
        // was stored as a global memory fact. Both migration sessions were given that
        // exact incantation.
        Some(p) if p.trim().is_empty() => Err(
            "PAOS_ROOT is set but EMPTY, so the store would silently fall back to the live \
             ~/.paos.\n  This is almost always a failed command substitution: `mktemp -d` \
             is denied in the agent sandbox.\n  Use an explicit path instead:\n    \
             ROOT=\"$TMPDIR/paos-iso-$$\"; mkdir -p \"$ROOT\"; PAOS_ROOT=\"$ROOT\" paos ..."
                .to_string(),
        ),
        Some(p) => Ok(PathBuf::from(p)),
        None => Ok(Path::new(home.unwrap_or(".")).join(".paos")),
    }
}

pub fn root() -> PathBuf {
    let env = std::env::var("PAOS_ROOT").ok();
    let home = std::env::var("HOME").ok();
    match resolve_root(env.as_deref(), home.as_deref()) {
        Ok(p) => p,
        // Refusing is the whole point: continuing would write the live store while the
        // caller believes they are isolated.
        Err(msg) => {
            eprintln!("paos: {msg}");
            std::process::exit(2);
        }
    }
}

pub fn db_path() -> PathBuf {
    root().join("paos.db")
}

pub fn socket_path() -> PathBuf {
    root().join("paosd.sock")
}

/// Open the database and bring it fully up to date.
///
/// Only the daemon calls this. Applying the ladder here — once, at startup — is what
/// lets read-only commands be genuinely read-only; the Python version ran 15
/// `PRAGMA table_info` probes plus an unconditional `UPDATE meta` + commit on *every*
/// process start, so even `paos bus whoami` dirtied the database and fsynced.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let conn = Connection::open(path)?;
    configure(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

/// An in-memory database with the full schema — the test seam the Python lacked.
/// `get_conn()` there took no arguments, so all 288 bus tests hit real disk.
pub fn open_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    configure(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

fn configure(conn: &Connection) -> Result<()> {
    // WAL: concurrent readers while the daemon writes.
    // synchronous=NORMAL: WAL-safe and removes the per-commit fsync. The Python code
    // never set this, so it inherited FULL and fsynced on every heartbeat — measured
    // 1,210 fsyncs/hour at idle with 5 sessions.
    // busy_timeout: config_facet connected with none at all and was called every 2s by
    // the daemon, which is a `database is locked` waiting to happen.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}

fn user_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// Apply every migration above the database's current version, in one transaction each.
pub fn migrate(conn: &Connection) -> Result<()> {
    let current = user_version(conn)?;
    if current > SCHEMA_VERSION {
        return Err(StoreError::FromTheFuture { found: current, known: SCHEMA_VERSION });
    }
    for (idx, sql) in migrations::LADDER.iter().enumerate() {
        let version = idx as i64 + 1;
        if version <= current {
            continue;
        }
        conn.execute_batch("BEGIN IMMEDIATE")?;
        match conn.execute_batch(sql) {
            Ok(()) => {
                // PRAGMA can't be parameterised.
                conn.execute_batch(&format!("PRAGMA user_version = {version}"))?;
                conn.execute_batch("COMMIT")?;
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e.into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_paos_root_is_refused_rather_than_silently_using_the_live_store() {
        // `PAOS_ROOT=$(mktemp -d)` with mktemp denied leaves PAOS_ROOT="". Treating that
        // as "unset" hands the caller ~/.paos while they believe they are isolated.
        let err = resolve_root(Some(""), Some("/home/u")).unwrap_err();
        assert!(err.contains("EMPTY"), "{err}");
        assert!(err.contains("mktemp"), "names the cause: {err}");
        assert!(resolve_root(Some("   "), Some("/home/u")).is_err(), "whitespace too");
    }

    #[test]
    fn an_unset_paos_root_still_falls_back_to_home() {
        assert_eq!(
            resolve_root(None, Some("/home/u")).unwrap(),
            PathBuf::from("/home/u/.paos")
        );
        assert_eq!(resolve_root(None, None).unwrap(), PathBuf::from("./.paos"));
    }

    #[test]
    fn an_explicit_paos_root_is_used_verbatim() {
        assert_eq!(
            resolve_root(Some("/tmp/iso"), Some("/home/u")).unwrap(),
            PathBuf::from("/tmp/iso")
        );
    }

    fn tables(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    #[test]
    fn fresh_database_lands_on_the_current_version() {
        let conn = open_in_memory().unwrap();
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn schema_version_matches_the_ladder_length() {
        // Appending a migration without bumping the constant would silently skip it.
        assert_eq!(SCHEMA_VERSION, migrations::LADDER.len() as i64);
    }

    #[test]
    fn one_schema_owner_creates_every_table() {
        // THE regression this crate exists to prevent: five partial owners meant the
        // first writer decided which tables existed.
        let conn = open_in_memory().unwrap();
        let t = tables(&conn);
        for expected in [
            "sessions", "rooms", "members", "messages", "cursors", "room_history",
            "task_log", "events", "operator_mode", "escalations", "parked",
            "operator_meta", "operator_outbox", "memory_proposals", "paos_config",
        ] {
            assert!(t.contains(&expected.to_string()), "missing table: {expected}");
        }
    }

    #[test]
    fn migrate_is_idempotent() {
        let conn = open_in_memory().unwrap();
        let before = tables(&conn);
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        assert_eq!(tables(&conn), before);
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn an_existing_database_gains_the_new_columns_without_losing_rows() {
        // The real risk with an ALTER-based migration: it runs against a LIVE database
        // with data in it, not the blank one the other tests use. A failed ALTER rolls
        // back and leaves paosd unable to open the store at all.
        // A RAW connection: open_in_memory() already migrates, so starting from it and
        // rewinding user_version would re-run the ALTER against an altered table.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(migrations::LADDER[0]).unwrap();
        conn.execute_batch("PRAGMA user_version = 1").unwrap();
        conn.execute_batch(
            "INSERT INTO memory_proposals(kind, dataset, text, status, created_ts) \
             VALUES('capture','ds','a pending fact','pending','2026-07-31T00:00:00Z')").unwrap();
        migrate(&conn).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_proposals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "an existing proposal must survive the migration");
        // NULL, i.e. "not screened" rather than "screened clean" — proposals queued
        // before the screen existed must not masquerade as vetted.
        let screen: Option<String> = conn
            .query_row("SELECT screen FROM memory_proposals", [], |r| r.get(0))
            .unwrap();
        assert!(screen.is_none());
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn a_newer_database_is_refused_rather_than_downgraded() {
        let conn = open_in_memory().unwrap();
        conn.execute_batch("PRAGMA user_version = 9999").unwrap();
        match migrate(&conn) {
            Err(StoreError::FromTheFuture { found, .. }) => assert_eq!(found, 9999),
            other => panic!("expected FromTheFuture, got {other:?}"),
        }
    }

    #[test]
    fn synchronous_is_normal_not_full() {
        // FULL was the inherited default and cost ~1,210 fsyncs/hour at idle.
        let conn = open_in_memory().unwrap();
        let sync: i64 = conn.query_row("PRAGMA synchronous", [], |r| r.get(0)).unwrap();
        assert_eq!(sync, 1, "expected NORMAL(1)");
    }

    #[test]
    fn messages_are_indexed_for_the_hot_path() {
        // `MAX(id) WHERE room=?` ran 1x/second/room/listener forever and, against the
        // (room, seq) index, scanned every entry for that room — the only query in the
        // system whose cost grew without bound.
        let conn = open_in_memory().unwrap();
        let plan: String = conn
            .query_row(
                "EXPLAIN QUERY PLAN SELECT MAX(id) FROM messages WHERE room = 'lobby'",
                [],
                |r| r.get(3),
            )
            .unwrap();
        assert!(
            plan.contains("idx_messages_room_id"),
            "MAX(id) must use the (room, id) index, got: {plan}"
        );
    }

    #[test]
    fn seq_is_unique_per_room() {
        // The Python allocated seq with SELECT MAX(seq)+1 then INSERT, trusting callers
        // to have opened BEGIN IMMEDIATE. Single-writer plus this constraint makes the
        // race impossible rather than merely unlikely.
        let conn = open_in_memory().unwrap();
        conn.execute("INSERT INTO rooms(room, created_ts) VALUES('lobby','t')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO messages(room, seq, ts, sender, target, text) \
             VALUES('lobby', 1, 't', 'a', '@all', 'hi')",
            [],
        )
        .unwrap();
        let dup = conn.execute(
            "INSERT INTO messages(room, seq, ts, sender, target, text) \
             VALUES('lobby', 1, 't', 'b', '@all', 'clash')",
            [],
        );
        assert!(dup.is_err(), "duplicate (room, seq) must be rejected");
    }
}
