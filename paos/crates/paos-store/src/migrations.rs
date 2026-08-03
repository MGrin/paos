//! The ordered migration ladder. **Append only** — never edit a shipped entry.
//!
//! Migration 1 reproduces the schema as it stood in the Python implementation on
//! 2026-07-29, so `paosd` opens an existing `~/.paos/paos.db` without touching a row:
//! every statement is `IF NOT EXISTS`, and a live database simply has its
//! `user_version` stamped. Migration 2 adds the indices the audit found missing.

/// Each entry is one migration, applied in order inside its own transaction.
pub const LADDER: &[&str] = &[MIGRATION_001_BASELINE, MIGRATION_002_HOT_PATH_INDICES,
                              MIGRATION_003_USAGE_SAMPLES, MIGRATION_004_PROPOSAL_SCREEN,
                              MIGRATION_005_TASKS];

/// The schema as the Python facets collectively created it.
///
/// Verbatim-compatible on purpose: this is what makes the cutover safe. `paosd` can be
/// started against the live database and, if anything goes wrong, stopped again with
/// the Python CLI still working on the same file.
const MIGRATION_001_BASELINE: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
  name TEXT PRIMARY KEY, status TEXT, updated_ts TEXT, stale_since TEXT,
  session_id TEXT, started_ts TEXT, last_seen TEXT, ended_ts TEXT, pid INTEGER,
  deaf_since TEXT, ack_skill_version TEXT);

CREATE TABLE IF NOT EXISTS rooms (
  room TEXT PRIMARY KEY, topic TEXT, created_ts TEXT, closed_ts TEXT,
  kind TEXT, repos TEXT);

CREATE TABLE IF NOT EXISTS members (
  room TEXT NOT NULL, name TEXT NOT NULL, ws_path TEXT, repo TEXT,
  pid INTEGER, joined_ts TEXT, last_seen TEXT, version TEXT,
  PRIMARY KEY (room, name));

CREATE TABLE IF NOT EXISTS messages (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  room TEXT NOT NULL, seq INTEGER NOT NULL, ts TEXT NOT NULL,
  sender TEXT NOT NULL, target TEXT NOT NULL, text TEXT NOT NULL,
  reply_to INTEGER, urgent INTEGER, ambient INTEGER,
  UNIQUE(room, seq));

CREATE TABLE IF NOT EXISTS cursors (
  room TEXT NOT NULL, member TEXT NOT NULL, seq INTEGER NOT NULL,
  PRIMARY KEY (room, member));

CREATE TABLE IF NOT EXISTS room_history (
  name TEXT NOT NULL, room TEXT NOT NULL, last_joined TEXT,
  PRIMARY KEY (name, room));

CREATE TABLE IF NOT EXISTS task_log (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_name TEXT NOT NULL, text TEXT, repo TEXT,
  started_ts TEXT, ended_ts TEXT, state TEXT, room TEXT);

CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);

CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY AUTOINCREMENT, ts TEXT NOT NULL, kind TEXT NOT NULL,
  session TEXT, summary TEXT NOT NULL, ref TEXT, data TEXT);

CREATE TABLE IF NOT EXISTS operator_mode (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  mode TEXT NOT NULL, updated_ts TEXT, set_by TEXT);

CREATE TABLE IF NOT EXISTS escalations (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session TEXT NOT NULL, question TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'open',
  answer TEXT, pushed INTEGER NOT NULL DEFAULT 0,
  nudged INTEGER NOT NULL DEFAULT 0,
  chat_id TEXT, message_id TEXT, waiter_seen TEXT,
  created_ts TEXT NOT NULL, answered_ts TEXT, options TEXT);

CREATE TABLE IF NOT EXISTS parked (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session TEXT NOT NULL, note TEXT NOT NULL,
  resolved INTEGER NOT NULL DEFAULT 0, created_ts TEXT NOT NULL);

CREATE TABLE IF NOT EXISTS operator_chat (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  text TEXT NOT NULL, consumed INTEGER NOT NULL DEFAULT 0,
  created_ts TEXT NOT NULL);

CREATE TABLE IF NOT EXISTS operator_meta (key TEXT PRIMARY KEY, value TEXT);

CREATE TABLE IF NOT EXISTS operator_outbox (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session TEXT NOT NULL, text TEXT NOT NULL,
  kind TEXT NOT NULL DEFAULT 'say',
  created_ts TEXT NOT NULL, sent_ts TEXT, message_id TEXT);

CREATE TABLE IF NOT EXISTS tg_message_map (
  message_id TEXT PRIMARY KEY, session TEXT NOT NULL, created_ts TEXT NOT NULL);

CREATE TABLE IF NOT EXISTS tg_topics (
  kind TEXT NOT NULL, key TEXT NOT NULL, thread_id INTEGER NOT NULL,
  title TEXT, closed_ts TEXT, created_ts TEXT NOT NULL,
  PRIMARY KEY (kind, key));

CREATE TABLE IF NOT EXISTS memory_proposals (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL, dataset TEXT NOT NULL, scope TEXT, text TEXT,
  target_data_id TEXT, rationale TEXT, source TEXT,
  status TEXT NOT NULL DEFAULT 'pending',
  created_ts TEXT NOT NULL, resolved_ts TEXT);

CREATE TABLE IF NOT EXISTS standup_briefs (
  id INTEGER PRIMARY KEY AUTOINCREMENT, ts TEXT NOT NULL, side TEXT NOT NULL,
  covers_from TEXT NOT NULL, covers_to TEXT NOT NULL, body TEXT NOT NULL,
  model TEXT, status TEXT NOT NULL DEFAULT 'draft');

CREATE TABLE IF NOT EXISTS standup_watermark (
  side TEXT PRIMARY KEY, reported_ts TEXT NOT NULL);

CREATE TABLE IF NOT EXISTS paos_config (
  key TEXT PRIMARY KEY, value TEXT, updated_ts TEXT);

CREATE INDEX IF NOT EXISTS idx_task_log_session ON task_log(session_name);
CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_session_id
  ON sessions(session_id) WHERE session_id IS NOT NULL;
"#;

/// Indices for the queries the audit measured as unbounded or scanning.
///
/// `idx_messages_room_seq` is deliberately dropped: `UNIQUE(room, seq)` already creates
/// exactly that index, so it was pure write amplification on the hottest table.
const MIGRATION_002_HOT_PATH_INDICES: &str = r#"
DROP INDEX IF EXISTS idx_messages_room_seq;

-- THE hot path: `SELECT MAX(id) FROM messages WHERE room=?`, run once per second per
-- room per listener. Against (room, seq) it scanned every entry for the room because
-- id is not in key order; this makes it a bounded index lookup.
CREATE INDEX IF NOT EXISTS idx_messages_room_id ON messages(room, id);
CREATE INDEX IF NOT EXISTS idx_messages_ts ON messages(ts);

-- 755 rows, 750 of them ended, scanned + temp-B-tree-sorted on every supervise tick
-- and every dashboard poll.
CREATE INDEX IF NOT EXISTS idx_sessions_active ON sessions(ended_ts, last_seen);
CREATE INDEX IF NOT EXISTS idx_members_name ON members(name);

-- "find the unprocessed rows" scans on daemon ticks.
CREATE INDEX IF NOT EXISTS idx_escalations_status ON escalations(status);
CREATE INDEX IF NOT EXISTS idx_parked_resolved ON parked(resolved);
CREATE INDEX IF NOT EXISTS idx_proposals_status ON memory_proposals(status);
CREATE INDEX IF NOT EXISTS idx_outbox_sent ON operator_outbox(sent_ts);
CREATE INDEX IF NOT EXISTS idx_tg_map_created ON tg_message_map(created_ts);

-- Unbounded and unindexed; read by the dashboard activity feed.
CREATE INDEX IF NOT EXISTS idx_events_kind_id ON events(kind, id);
"#;

/// Usage history, so the accounts view can say WHEN a window was consumed rather than
/// only that it is gone.
///
/// Correlation, not attribution: Anthropic reports per-ACCOUNT totals and no session
/// reports its token spend to paos. This records the climb; naming a culprit would be a
/// guess dressed as a fact.
const MIGRATION_003_USAGE_SAMPLES: &str = r#"
CREATE TABLE IF NOT EXISTS usage_samples (
  ts         INTEGER NOT NULL,
  slot       TEXT NOT NULL,
  five_hour  REAL NOT NULL,
  seven_day  REAL NOT NULL,
  PRIMARY KEY (ts, slot)
);
CREATE INDEX IF NOT EXISTS idx_usage_ts ON usage_samples(ts);
"#;

/// An advisory quality flag on a proposal, so review can be triaged rather than read
/// linearly.
///
/// Deliberately advisory and NEVER an auto-reject. Measured against the 40 historically
/// reviewed dream captures, the stated capture policy and the human's actual decisions
/// disagree in BOTH directions: facts the policy says to keep (an external-system quirk,
/// "piping masks a command's exit code") were rejected, while facts it calls status ("the
/// fix is pushed at SHA ...", "synced 922 records") were approved. So there is no reliable
/// label to calibrate an automatic filter against, and a screen that dropped proposals
/// would have silently discarded good ones.
///
/// What it can do safely is sort: put the obvious status/version noise last and say why,
/// so the queue is fast to scan. The human still decides every case.
const MIGRATION_004_PROPOSAL_SCREEN: &str = r#"
ALTER TABLE memory_proposals ADD COLUMN screen TEXT;
ALTER TABLE memory_proposals ADD COLUMN screen_why TEXT;
"#;

/// The fleet's shared work queue: what work exists, who holds it, and what happened to
/// it. Memory is for facts that stay true; a half-finished refactor is not one, and
/// sessions had nowhere else to put it.
///
/// **No FOREIGN KEY clauses**, though `configure()` does set `PRAGMA foreign_keys=ON`.
/// Not an oversight and not because enforcement is off: no other table here declares
/// one, so `tasks` would be the only place in the database where a delete could fail on
/// a constraint. `parent_id` and `depends_on` integrity is checked in `paos-tasks` at
/// write time, which is where the error message can say what to do about it.
///
/// `state` deliberately has no `blocked` value. Blocked is a query over unmet
/// dependencies — a stored flag needs something to keep it true, and nothing would.
const MIGRATION_005_TASKS: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
  id           TEXT PRIMARY KEY,
  title        TEXT NOT NULL,
  body         TEXT,
  state        TEXT NOT NULL,
  priority     INTEGER NOT NULL DEFAULT 2,
  scope        TEXT NOT NULL,
  org          TEXT,
  repo         TEXT,
  parent_id    TEXT,
  origin       TEXT NOT NULL,
  created_by   TEXT NOT NULL,
  claimed_by   TEXT,
  claimed_ts   TEXT,
  last_owner   TEXT,
  orphaned     INTEGER NOT NULL DEFAULT 0,
  close_grant  INTEGER NOT NULL DEFAULT 0,
  room         TEXT,
  created_ts   TEXT NOT NULL,
  updated_ts   TEXT NOT NULL,
  closed_ts    TEXT);

CREATE TABLE IF NOT EXISTS task_deps (
  task_id     TEXT NOT NULL,
  depends_on  TEXT NOT NULL,
  kind        TEXT NOT NULL DEFAULT 'blocks',
  PRIMARY KEY (task_id, depends_on));

-- Its own table rather than rows in `events`, because `paos event prune --days 30`
-- would delete these out from under a task. A rescuing session reads this log to learn
-- what the dead session had already done; nothing on a timer may point at it.
CREATE TABLE IF NOT EXISTS task_notes (
  id       INTEGER PRIMARY KEY AUTOINCREMENT,
  task_id  TEXT NOT NULL,
  ts       TEXT NOT NULL,
  author   TEXT NOT NULL,
  kind     TEXT NOT NULL DEFAULT 'note',
  text     TEXT NOT NULL);

CREATE INDEX IF NOT EXISTS idx_tasks_state ON tasks(state, priority, created_ts);
CREATE INDEX IF NOT EXISTS idx_tasks_claimed ON tasks(claimed_by) WHERE claimed_by IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_tasks_parent ON tasks(parent_id) WHERE parent_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_tasks_repo ON tasks(repo);
CREATE INDEX IF NOT EXISTS idx_task_notes_task ON task_notes(task_id, id);
CREATE INDEX IF NOT EXISTS idx_task_deps_dep ON task_deps(depends_on);
"#;

#[cfg(test)]
mod tests {
    /// Through the real `migrate()`, not by replaying LADDER — migration 004 is
    /// `ALTER TABLE ADD COLUMN`, which errors on a second application. `user_version`
    /// is what makes the ladder idempotent, so a test that bypasses it tests nothing
    /// the daemon actually does.
    #[test]
    fn migration_005_creates_the_task_tables() {
        let conn = crate::open_in_memory().unwrap();
        conn.execute(
            "INSERT INTO tasks(id,title,state,priority,scope,origin,created_by,\
             created_ts,updated_ts) \
             VALUES('t-aaaaaa','x','ready',2,'global','session','swift-otter',\
             '2026-08-01','2026-08-01')", []).unwrap();
        conn.execute("INSERT INTO task_deps(task_id,depends_on) VALUES('t-aaaaaa','t-bbbbbb')",
                     []).unwrap();
        conn.execute("INSERT INTO task_notes(task_id,ts,author,text) \
                      VALUES('t-aaaaaa','2026-08-01','operator','hi')", []).unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    /// A dep row may reference a task that does not exist yet. This is the FK decision
    /// made observable: were a FOREIGN KEY declared, the insert above would fail, and
    /// `paos-tasks` could not give the caller a better message than SQLITE_CONSTRAINT.
    #[test]
    fn the_task_tables_declare_no_foreign_keys() {
        let conn = crate::open_in_memory().unwrap();
        let fk_on: i64 = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0)).unwrap();
        assert_eq!(fk_on, 1, "enforcement is on database-wide; this test is about declarations");
        conn.execute("INSERT INTO task_deps(task_id,depends_on) VALUES('t-ghost','t-also-ghost')",
                     []).unwrap();
    }
}
