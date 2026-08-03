//! `paos event` — the cross-facet activity journal.
//!
//! Ported from `events_facet.py`. Appends go through the daemon's `Event` verb, falling
//! back to a direct append when the socket is blocked: `events` is append-only with an
//! autoincrement id, and SQLite in WAL mode with a busy_timeout handles concurrent
//! appends correctly. That fallback is deliberate and was arrived at by measurement —
//! routing 739 events a day through the spool would write, read and delete 739 files and
//! add up to 5s of lag to a journal, and the single-writer rule exists to protect the
//! EMBEDDING and the schema, neither of which applies here. Memory writes still go
//! through the daemon or the spool, always.

use paos_proto::{Request, Response};

const DEFAULT_LIMIT: i64 = 50;
const DEFAULT_PRUNE_DAYS: i64 = 30;

fn ro() -> Option<rusqlite::Connection> {
    rusqlite::Connection::open_with_flags(
        paos_store::db_path(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
}

pub fn run(positional: &[String], args: &[String],
           send: impl Fn(&Request) -> Option<Response>) -> i32 {
    let opt = |n: &str| args.iter().position(|a| a == n).and_then(|i| args.get(i + 1))
        .map(String::as_str);
    match positional.get(1).map(String::as_str).unwrap_or("log") {
        "record" => {
            let Some(kind) = positional.get(2) else {
                eprintln!("event record needs <kind> <summary>"); return 2 };
            let summary = positional.get(3).cloned().unwrap_or_default();
            record(kind, &summary, opt("--session"), opt("--ref"), opt("--data"), &send)
        }
        "log" => log(opt("--kind"), opt("--session"),
                     opt("--limit").and_then(|l| l.parse().ok()).unwrap_or(DEFAULT_LIMIT)),
        "prune" => prune(opt("--days").and_then(|d| d.parse().ok())
                         .unwrap_or(DEFAULT_PRUNE_DAYS), &send),
        other => {
            eprintln!("unknown event subcommand: {other}\n\
                       usage: paos event [record <kind> <summary> | log | prune]");
            2
        }
    }
}

fn record(kind: &str, summary: &str, session: Option<&str>, reference: Option<&str>,
          data: Option<&str>, send: &impl Fn(&Request) -> Option<Response>) -> i32 {
    let req = Request::Event {
        kind: kind.into(), summary: summary.into(),
        session: session.map(str::to_string),
        reference: reference.map(str::to_string),
        data: data.map(str::to_string),
    };
    match send(&req) {
        Some(Response::Ok { .. }) => { println!("recorded"); 0 }
        Some(Response::Err { message, exit_code }) => { eprintln!("{message}"); exit_code }
        None => direct_append(kind, summary, session, reference, data),
    }
}

/// The daemon is unreachable — append directly. See the module docstring for why this is
/// the one write in paos that legitimately bypasses it.
fn direct_append(kind: &str, summary: &str, session: Option<&str>, reference: Option<&str>,
                 data: Option<&str>) -> i32 {
    let Ok(c) = rusqlite::Connection::open(paos_store::db_path()) else {
        eprintln!("paos.db unwritable — event not recorded");
        return 1;
    };
    let _ = c.busy_timeout(std::time::Duration::from_secs(5));
    match c.execute(
        "INSERT INTO events(ts, kind, session, summary, ref, data) VALUES(?1,?2,?3,?4,?5,?6)",
        rusqlite::params![super::now_iso(), kind, session, summary, reference, data]) {
        Ok(_) => { println!("recorded"); 0 }
        Err(e) => { eprintln!("event write failed: {e}"); 1 }
    }
}

fn log(kind: Option<&str>, session: Option<&str>, limit: i64) -> i32 {
    let Some(c) = ro() else { eprintln!("paos.db unreadable"); return 1 };
    match log_lines(&c, kind, session, limit) {
        Some(lines) => { for l in lines { println!("{l}"); } 0 }
        None => { eprintln!("query failed"); 1 }
    }
}

/// Split from `log` so it is testable against a scratch database rather than the
/// caller's real one.
fn log_lines(c: &rusqlite::Connection, kind: Option<&str>, session: Option<&str>,
             limit: i64) -> Option<Vec<String>> {
    let mut sql = String::from(
        "SELECT ts, kind, session, summary FROM events");
    let mut clauses: Vec<&str> = vec![];
    if kind.is_some() { clauses.push("kind LIKE ?"); }
    if session.is_some() { clauses.push("session = ?"); }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY id DESC LIMIT ?");

    let mut params: Vec<String> = vec![];
    if let Some(k) = kind { params.push(format!("{k}%")); }   // prefix match, as the Python
    if let Some(s) = session { params.push(s.to_string()); }
    params.push(limit.to_string());

    let mut st = c.prepare(&sql).ok()?;
    let rows = st.query_map(rusqlite::params_from_iter(params.iter()), |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?, r.get::<_, String>(3)?))
    }).ok()?;
    Some(rows.flatten()
        .map(|(ts, kind, session, summary)| {
            format!("{ts} [{kind}] {}: {summary}", session.as_deref().unwrap_or("-"))
        })
        .collect())
}

fn prune(days: i64, send: &impl Fn(&Request) -> Option<Response>) -> i32 {
    match send(&Request::EventPrune { days }) {
        Some(Response::Ok { lines }) => { for l in lines { println!("{l}"); } 0 }
        Some(Response::Err { message, exit_code }) => { eprintln!("{message}"); exit_code }
        // A DELETE is not an append: it is the one events operation that must not race
        // the daemon, so it does not get the direct fallback that `record` has.
        None => {
            eprintln!("paos: cannot reach paosd — prune needs the daemon");
            super::EXIT_NO_DAEMON
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded() -> rusqlite::Connection {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE events(id INTEGER PRIMARY KEY AUTOINCREMENT, ts TEXT, kind TEXT, \
             session TEXT, summary TEXT, ref TEXT, data TEXT);").unwrap();
        for (ts, kind, session, summary) in [
            ("2026-07-31T01:00:00Z", "session.online",  Some("alice"), "up"),
            ("2026-07-31T02:00:00Z", "memory.remembered", Some("bob"), "stored a fact"),
            ("2026-07-31T03:00:00Z", "memory.forgotten", None,         "forgot one"),
        ] {
            c.execute("INSERT INTO events(ts,kind,session,summary) VALUES(?1,?2,?3,?4)",
                      rusqlite::params![ts, kind, session, summary]).unwrap();
        }
        c
    }

    #[test]
    fn newest_first_because_the_tail_is_what_you_want() {
        let c = seeded();
        let out = log_lines(&c, None, None, 50).unwrap();
        assert!(out[0].contains("forgot one"), "{out:?}");
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn kind_is_a_prefix_match_not_an_exact_one() {
        // `--kind memory` must catch memory.remembered AND memory.forgotten; requiring
        // the full dotted kind would make the filter useless for a whole facet.
        let c = seeded();
        assert_eq!(log_lines(&c, Some("memory"), None, 50).unwrap().len(), 2);
        assert_eq!(log_lines(&c, Some("session"), None, 50).unwrap().len(), 1);
    }

    #[test]
    fn session_is_an_exact_match() {
        let c = seeded();
        let out = log_lines(&c, None, Some("alice"), 50).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("up"));
    }

    #[test]
    fn filters_combine_rather_than_replacing_each_other() {
        let c = seeded();
        assert_eq!(log_lines(&c, Some("memory"), Some("bob"), 50).unwrap().len(), 1);
        assert_eq!(log_lines(&c, Some("session"), Some("bob"), 50).unwrap().len(), 0);
    }

    #[test]
    fn the_limit_is_applied() {
        let c = seeded();
        assert_eq!(log_lines(&c, None, None, 2).unwrap().len(), 2);
    }

    #[test]
    fn a_null_session_renders_as_a_dash_not_as_the_word_none() {
        let c = seeded();
        assert!(log_lines(&c, None, None, 1).unwrap()[0].contains("] -: "),
                "a Rust Option printed raw would read 'None'");
    }

    #[test]
    fn a_missing_table_is_an_error_not_an_empty_journal() {
        // "no events" and "cannot read events" must not look the same: one is a quiet
        // day, the other is a broken install.
        let c = rusqlite::Connection::open_in_memory().unwrap();
        assert!(log_lines(&c, None, None, 10).is_none());
    }
}
