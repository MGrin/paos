//! The operator channel: reaching the human, and deciding when we're allowed to.
//!
//! The gating rule is the whole point of this crate and it is deliberately dumb:
//! **Telegram is opt-in, never inferred.** An earlier build derived "away" from
//! HIDIdleTime, so any ten-minute break turned the phone into a pager. It was removed
//! on request. Do not add presence heuristics back — `away_state` has exactly two
//! doors, both opened by the operator.

use rusqlite::{params, Connection};

pub mod accounts;
pub mod poll;
pub mod switch;
pub mod slots;
pub mod usage;
pub mod telegram;

/// How long an inbound Telegram message keeps the channel open.
///
/// This one *must* expire: it asserts "they are on their phone right now", which stops
/// being true. Away mode, by contrast, must NOT expire — see `away_state`.
pub const TELEGRAM_ACTIVE_SECS: i64 = 30 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// At the laptop — ask in the terminal.
    Attended,
    /// At the laptop, hands-off — proceed on routine work. NOT "page me".
    Autonomous,
    /// Telegram is the channel.
    Away,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "attended" => Some(Mode::Attended),
            "autonomous" => Some(Mode::Autonomous),
            "away" => Some(Mode::Away),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Attended => "attended",
            Mode::Autonomous => "autonomous",
            Mode::Away => "away",
        }
    }
}

pub fn get_mode(conn: &Connection) -> Mode {
    conn.query_row("SELECT mode FROM operator_mode WHERE id=1", [], |r| r.get::<_, String>(0))
        .ok()
        .and_then(|s| Mode::parse(&s))
        .unwrap_or(Mode::Attended)
}

pub fn set_mode(conn: &Connection, mode: Mode, by: &str, now: &str) -> rusqlite::Result<bool> {
    let old = get_mode(conn);
    conn.execute(
        "INSERT INTO operator_mode(id, mode, updated_ts, set_by) VALUES(1, ?1, ?2, ?3) \
         ON CONFLICT(id) DO UPDATE SET mode=excluded.mode, updated_ts=excluded.updated_ts, \
         set_by=excluded.set_by",
        params![mode.as_str(), now, by],
    )?;
    Ok(old != mode)
}

/// Record that the operator just spoke to us on Telegram. Opens door 2.
pub fn mark_operator_seen(conn: &Connection, now: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO operator_meta(key, value) VALUES('last_operator_ts', ?1)",
        [now],
    )?;
    Ok(())
}

/// May we emit to Telegram right now?
///
/// Two doors, both opened by the operator themselves:
///   1. they set `away` mode — a **latch with no TTL**, because an expiring away
///      silently strands them off-channel, which is the dangerous direction;
///   2. they messaged the bot within `TELEGRAM_ACTIVE_SECS` — an answer belongs in the
///      thread they started.
///
/// Everything else is silent. `Autonomous` means hands-off, not "page me".
pub fn may_push(conn: &Connection, now_epoch: i64) -> bool {
    if get_mode(conn) == Mode::Away {
        return true;
    }
    match last_operator_epoch(conn) {
        Some(t) => now_epoch.saturating_sub(t) <= TELEGRAM_ACTIVE_SECS,
        None => false,
    }
}

fn last_operator_epoch(conn: &Connection) -> Option<i64> {
    let s: String = conn
        .query_row("SELECT value FROM operator_meta WHERE key='last_operator_ts'", [], |r| r.get(0))
        .ok()?;
    parse_iso_epoch(&s)
}

/// Parse `YYYY-MM-DDTHH:MM:SSZ` to a unix epoch. Returns None on anything else, and a
/// None here means "no recent activity" — failing closed keeps the phone quiet.
pub fn parse_iso_epoch(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |a: usize, z: usize| -> Option<i64> { s.get(a..z)?.parse().ok() };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    // days_from_civil (Howard Hinnant)
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + sec)
}

// --- escalations -------------------------------------------------------------

pub fn ask(conn: &Connection, session: &str, question: &str, options: Option<&str>, now: &str)
    -> rusqlite::Result<i64>
{
    conn.execute(
        "INSERT INTO escalations(session, question, status, created_ts, options) \
         VALUES(?1, ?2, 'open', ?3, ?4)",
        params![session, question, now, options],
    )?;
    Ok(conn.last_insert_rowid())
}

/// On a transition INTO away, turn sessions already blocked in their terminal into
/// escalations, so their questions reach Telegram. Returns the handles escalated.
///
/// **This was lost in the Rust cutover and nothing noticed**, which is the failure mode
/// this whole system is prone to: a capability that stops working without erroring. The
/// Python `paos-operatord` swept on every mode change; the Rust bridge only ever grew a
/// `/blocked` command the operator has to think to run. So the operator would go AFK and
/// every session already waiting on them stayed silently blocked — the exact situation
/// away mode exists to prevent.
///
/// Only on the TRANSITION: sweeping every tick while away would re-escalate a question
/// the operator is deliberately ignoring. Dedupes against open escalations for the same
/// reason.
pub fn sweep_blocked_on_away(conn: &Connection, was: Mode, now_mode: Mode, now: &str)
    -> rusqlite::Result<Vec<String>>
{
    if now_mode != Mode::Away || was == Mode::Away {
        return Ok(vec![]);
    }
    let already: std::collections::HashSet<String> =
        open_escalations(conn)?.into_iter().map(|(_, s, _)| s).collect();
    let mut stmt = conn.prepare(
        "SELECT name, COALESCE(status,'') FROM sessions WHERE ended_ts IS NULL")?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .flatten()
        .collect();
    let mut escalated = vec![];
    for (name, status) in rows {
        if !is_blocked_status(&status) || already.contains(&name) {
            continue;
        }
        // Strip the marker so the operator reads the question, not the decoration.
        let q = status.trim_start_matches('⛔').trim();
        let q = q.strip_prefix("blocked:").unwrap_or(q).trim();
        let q = if q.is_empty() { "session is blocked (no question text)" } else { q };
        ask(conn, &name, q, None, now)?;
        escalated.push(name);
    }
    Ok(escalated)
}

/// The two ways a session marks itself blocked. Both are written by `paos bus blocked`.
fn is_blocked_status(status: &str) -> bool {
    status.starts_with('⛔') || status.to_lowercase().starts_with("blocked:")
}

/// Record an answer AND deliver it to the session that asked.
///
/// Answering used to be only the UPDATE, and nothing told the asker. A session's only
/// ear is its bus listener, so the answer sat in a row until the session thought to look
/// — which it generally does not, because the whole point of `ask` is that it went away
/// to do something else. Measured 2026-08-03: escalations 43 and 44 were answered within
/// minutes and the asking session found out only by reading sqlite by hand, after the
/// operator asked why nothing had happened.
///
/// The `session` column was there the whole time. Delivery belongs HERE rather than at
/// the four call sites (Telegram button, Telegram quote-reply, dashboard, CLI) so no
/// path can answer without telling anyone.
///
/// Addressed to the session, so it WAKES that listener — an `@all` post would not. Sent
/// as `operator` because it IS the operator speaking; that sender is the one a peer
/// cannot impersonate.
pub fn answer(conn: &mut Connection, id: i64, text: &str, now: &str) -> rusqlite::Result<bool> {
    let changed = conn.execute(
        "UPDATE escalations SET status='answered', answer=?1, answered_ts=?2 \
         WHERE id=?3 AND status='open'",
        params![text, now, id],
    )? > 0;
    if !changed {
        return Ok(false);
    }
    let session: Option<String> = conn
        .query_row("SELECT session FROM escalations WHERE id=?1", [id], |r| r.get(0))
        .ok();
    if let Some(s) = session.filter(|s| !s.trim().is_empty()) {
        // Best-effort: the answer is already recorded, and failing to announce it must
        // not turn a delivered decision into an error the operator sees.
        let _ = paos_bus::post(
            conn, "lobby", "operator", &format!("@{s}"),
            &format!("📱 operator: [answer to #{id}] {text}"), now, false, false);
    }
    Ok(true)
}

pub fn open_escalations(conn: &Connection) -> rusqlite::Result<Vec<(i64, String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, session, question FROM escalations WHERE status='open' ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    rows.collect()
}

/// Escalations that have not been pushed yet, for the daemon to deliver.
pub fn unpushed(conn: &Connection) -> rusqlite::Result<Vec<(i64, String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, session, question FROM escalations \
         WHERE status='open' AND pushed=0 ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    rows.collect()
}

pub fn mark_pushed(conn: &Connection, id: i64, message_id: Option<&str>) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE escalations SET pushed=1, message_id=?1 WHERE id=?2",
        params![message_id, id],
    )?;
    Ok(())
}

/// Escalation options as stored (comma-joined).
pub fn escalation_options(conn: &Connection, id: i64) -> Vec<String> {
    conn.query_row("SELECT options FROM escalations WHERE id=?1", [id], |r| {
        r.get::<_, Option<String>>(0)
    })
    .ok()
    .flatten()
    .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
    .unwrap_or_default()
}

/// Close an escalation without an answer — the session stopped needing it.
///
/// Distinct from `answer`: 13 of the 30 escalations on this machine were closed this way,
/// so it is the normal end for a question the work moved past.
pub fn resolve(conn: &Connection, id: i64, now: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE escalations SET status='resolved', answered_ts=?1 WHERE id=?2 AND status='open'",
        params![now, id],
    )? > 0)
}

pub fn park(conn: &Connection, session: &str, note: &str, now: &str) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO parked(session, note, created_ts) VALUES(?1, ?2, ?3)",
        params![session, note, now],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Queue a message for the operator's phone. The daemon drains it.
pub fn enqueue_say(conn: &Connection, session: &str, text: &str, now: &str)
    -> rusqlite::Result<i64>
{
    conn.execute(
        "INSERT INTO operator_outbox(session, text, kind, created_ts) VALUES(?1, ?2, 'say', ?3)",
        params![session, text, now],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Record something the OPERATOR said, for a session to pick up.
pub fn record_operator_message(conn: &Connection, text: &str, now: &str) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO operator_chat(text, created_ts) VALUES(?1, ?2)",
        params![text, now],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Take the oldest unconsumed operator message, marking it consumed.
///
/// FIFO and consuming: two sessions listening must not both receive the same message and
/// both act on it.
pub fn take_operator_message(conn: &Connection) -> rusqlite::Result<Option<String>> {
    let row: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, text FROM operator_chat WHERE consumed=0 ORDER BY id LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    match row {
        None => Ok(None),
        Some((id, text)) => {
            conn.execute("UPDATE operator_chat SET consumed=1 WHERE id=?1", [id])?;
            Ok(Some(text))
        }
    }
}

/// The answer to an escalation, once it has one. `(status, answer)`.
pub fn escalation_state(conn: &Connection, id: i64) -> Option<(String, Option<String>)> {
    conn.query_row("SELECT status, answer FROM escalations WHERE id=?1", [id], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })
    .ok()
}

pub fn open_parked(conn: &Connection) -> rusqlite::Result<Vec<(i64, String, String)>> {
    let mut st = conn.prepare(
        "SELECT id, session, note FROM parked WHERE resolved=0 ORDER BY id")?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    rows.collect()
}

pub fn resolve_park(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute("UPDATE parked SET resolved=1 WHERE id=?1 AND resolved=0", [id])? > 0)
}

/// Queued `paos operator say` messages awaiting delivery.
///
/// The Rust bridge never drained this, so every `say` vanished into SQLite silently.
pub fn unsent_outbox(conn: &Connection) -> rusqlite::Result<Vec<(i64, String, String)>> {
    let mut st = conn.prepare(
        "SELECT id, session, text FROM operator_outbox WHERE sent_ts IS NULL ORDER BY id LIMIT 10")?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    rows.collect()
}

pub fn mark_outbox_sent(conn: &Connection, id: i64, now: &str, message_id: Option<i64>)
    -> rusqlite::Result<()>
{
    conn.execute(
        "UPDATE operator_outbox SET sent_ts=?1, message_id=?2 WHERE id=?3",
        params![now, message_id.map(|m| m.to_string()), id],
    )?;
    Ok(())
}

/// Remember which session a Telegram message came from, so a quote-reply can be routed
/// back to it. Without this map, replying to a session's message reaches nobody.
pub fn record_tg_message(conn: &Connection, message_id: i64, session: &str, now: &str)
    -> rusqlite::Result<()>
{
    conn.execute(
        "INSERT OR REPLACE INTO tg_message_map(message_id, session, created_ts) \
         VALUES(?1, ?2, ?3)",
        params![message_id.to_string(), session, now],
    )?;
    Ok(())
}

pub fn session_by_message_id(conn: &Connection, message_id: i64) -> Option<String> {
    conn.query_row(
        "SELECT session FROM tg_message_map WHERE message_id=?1",
        [message_id.to_string()],
        |r| r.get(0),
    )
    .ok()
}

pub fn escalation_by_message_id(conn: &Connection, message_id: i64) -> Option<i64> {
    conn.query_row(
        "SELECT id FROM escalations WHERE message_id=?1 AND status='open'",
        [message_id.to_string()],
        |r| r.get(0),
    )
    .ok()
}

pub fn set_escalation_message_id(conn: &Connection, id: i64, message_id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE escalations SET message_id=?1 WHERE id=?2",
        params![message_id.to_string(), id],
    )?;
    Ok(())
}

/// Match a loose token from Telegram against a live session handle.
///
/// The operator types `@tashkent`, not `@owner/tashkent`. An exact match wins; else a
/// unique suffix match; ambiguity resolves to None rather than guessing wrong.
pub fn resolve_session(conn: &Connection, token: &str) -> Option<String> {
    let t = token.trim().trim_start_matches('@').to_lowercase();
    if t.is_empty() {
        return None;
    }
    let mut st = conn
        .prepare("SELECT name FROM sessions WHERE ended_ts IS NULL")
        .ok()?;
    let names: Vec<String> = st
        .query_map([], |r| r.get::<_, String>(0))
        .ok()?
        .filter_map(Result::ok)
        .collect();
    if let Some(exact) = names.iter().find(|n| n.to_lowercase() == t) {
        return Some(exact.clone());
    }
    let hits: Vec<&String> = names
        .iter()
        .filter(|n| n.to_lowercase().ends_with(&format!("/{t}")) || n.to_lowercase().ends_with(&format!("-{t}")))
        .collect();
    if hits.len() == 1 {
        Some(hits[0].clone())
    } else {
        None
    }
}

/// The "what needs me?" summary.
pub fn digest(conn: &Connection) -> String {
    let esc = open_escalations(conn).unwrap_or_default();
    let parked = open_parked(conn).unwrap_or_default();
    let proposals: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_proposals WHERE status='pending'", [], |r| r.get(0))
        .unwrap_or(0);
    let deaf: Vec<String> = {
        let mut st = match conn.prepare(
            "SELECT name FROM sessions WHERE ended_ts IS NULL AND deaf_since IS NOT NULL") {
            Ok(s) => s,
            Err(_) => return "✓ all quiet".into(),
        };
        st.query_map([], |r| r.get::<_, String>(0))
            .map(|it| it.filter_map(Result::ok).collect())
            .unwrap_or_default()
    };
    // Tasks awaiting the operator — review items they must approve, and work the fleet
    // dropped. Absent until 2026-08-03, which made the whole facet invisible here: a task
    // sitting in `review` for a decision only they can make showed up nowhere they look,
    // unless they remembered to type /tasks.
    //
    // It has to be part of the all-quiet test, not just an extra line. Reporting
    // "✓ all quiet — nothing needs you" while a task waits is a lie told by the one view
    // whose entire job is to be trusted when it says there is nothing.
    let tasks = paos_tasks::query::needs_operator(conn).unwrap_or(0);
    if esc.is_empty() && parked.is_empty() && proposals == 0 && deaf.is_empty() && tasks == 0 {
        return "✓ all quiet — nothing needs you.".into();
    }
    let mut out = vec!["📋 PAOS digest".to_string()];
    for (id, session, q) in &esc {
        out.push(format!("• #{id} [{session}] {q}"));
    }
    if !parked.is_empty() {
        out.push(format!("• {} parked decision(s)", parked.len()));
    }
    if tasks > 0 {
        out.push(format!("• {tasks} task(s) waiting on you"));
    }
    if proposals > 0 {
        out.push(format!("• {proposals} memory proposal(s) pending"));
    }
    if !deaf.is_empty() {
        out.push(format!("• ⚠ DEAF: {}", deaf.join(", ")));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        paos_store::open_in_memory().unwrap()
    }
    const T0: i64 = 1_785_000_000;
    fn iso(epoch: i64) -> String {
        // round-trip helper built on the parser's inverse expectations
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

    #[test]
    fn default_is_attended_and_silent() {
        let mut c = db();
        assert_eq!(get_mode(&c), Mode::Attended);
        assert!(!may_push(&c, T0), "a fresh machine must not page the phone");
    }

    #[test]
    fn away_opens_the_channel_and_never_expires() {
        // A latch: an expiring away silently strands the operator off-channel.
        let mut c = db();
        set_mode(&c, Mode::Away, "test", &iso(T0)).unwrap();
        for later in [T0, T0 + 3600, T0 + 86_400 * 30] {
            assert!(may_push(&c, later), "away must hold at +{}s", later - T0);
        }
    }

    #[test]
    fn autonomous_is_hands_off_not_page_me() {
        let mut c = db();
        set_mode(&c, Mode::Autonomous, "test", &iso(T0)).unwrap();
        assert!(!may_push(&c, T0));
    }

    #[test]
    fn a_recent_telegram_message_opens_the_channel() {
        let mut c = db();
        mark_operator_seen(&c, &iso(T0)).unwrap();
        assert!(may_push(&c, T0 + 60));
    }

    #[test]
    fn the_telegram_window_expires_unlike_away() {
        // It claims "they are on their phone now", which stops being true.
        let mut c = db();
        mark_operator_seen(&c, &iso(T0)).unwrap();
        assert!(may_push(&c, T0 + TELEGRAM_ACTIVE_SECS - 1));
        assert!(!may_push(&c, T0 + TELEGRAM_ACTIVE_SECS + 1));
    }

    #[test]
    fn no_idle_time_heuristic_exists() {
        // The regression this crate is shaped to prevent: inferring "away" from idle
        // time turned every coffee break into a page.
        // Scan EXECUTABLE code only: skip this test module (its assertion list would
        // match itself) and skip comments (the module docs legitimately explain why the
        // heuristic was removed). Both bit me in turn.
        let src = include_str!("lib.rs");
        let code: String = src
            .split("#[cfg(test)]")
            .next()
            .unwrap_or("")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for banned in ["HIDIdleTime", "ioreg", "idle_seconds"] {
            assert!(!code.contains(banned), "presence heuristic crept back in: {banned}");
        }
        assert!(code.contains("may_push"), "sanity: we are scanning the real source");
    }

    #[test]
    fn an_unparseable_timestamp_fails_closed() {
        let mut c = db();
        c.execute("INSERT INTO operator_meta(key,value) VALUES('last_operator_ts','nonsense')", [])
            .unwrap();
        assert!(!may_push(&c, T0), "unknown state must keep the phone quiet");
    }

    #[test]
    fn iso_parsing_round_trips_and_rejects_junk() {
        assert_eq!(parse_iso_epoch(&iso(T0)), Some(T0));
        assert_eq!(parse_iso_epoch("2026-07-29T11:37:35Z"), parse_iso_epoch("2026-07-29T11:37:35Z"));
        for bad in ["", "nope", "2026-13-01T00:00:00Z", "2026-07-32T00:00:00Z", "2026-07"] {
            assert_eq!(parse_iso_epoch(bad), None, "should reject {bad:?}");
        }
    }

    #[test]
    fn mode_change_is_reported_only_when_it_actually_changes() {
        // The banner broadcasts on change; re-setting the same mode must stay quiet.
        let mut c = db();
        assert!(set_mode(&c, Mode::Away, "t", &iso(T0)).unwrap());
        assert!(!set_mode(&c, Mode::Away, "t", &iso(T0)).unwrap());
        assert!(set_mode(&c, Mode::Attended, "t", &iso(T0)).unwrap());
    }

    // --- the away sweep -------------------------------------------------------
    //
    // This capability was LOST in the Rust cutover and nothing reported it: the operator
    // would go AFK and every session already waiting on them stayed silently blocked,
    // which is exactly what away mode exists to prevent.

    fn blocked_session(c: &Connection, name: &str, status: &str) {
        c.execute("INSERT INTO sessions(name, status, updated_ts) VALUES(?1,?2,'t')",
                  params![name, status]).unwrap();
    }

    #[test]
    fn going_away_escalates_a_blocked_session() {
        let mut c = db();
        blocked_session(&c, "quiet-otter", "⛔ which currency should the report use?");
        let out = sweep_blocked_on_away(&c, Mode::Attended, Mode::Away, "t").unwrap();
        assert_eq!(out, vec!["quiet-otter"]);
        let open = open_escalations(&c).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].2, "which currency should the report use?",
                   "the marker is decoration; the operator should read the question");
    }

    #[test]
    fn the_plain_text_blocked_prefix_is_recognised_too() {
        let mut c = db();
        blocked_session(&c, "s", "blocked: needs a decision");
        assert_eq!(sweep_blocked_on_away(&c, Mode::Attended, Mode::Away, "t").unwrap().len(), 1);
        assert_eq!(open_escalations(&c).unwrap()[0].2, "needs a decision");
    }

    #[test]
    fn a_working_session_is_not_escalated() {
        let mut c = db();
        blocked_session(&c, "busy", "refactoring the parser");
        assert!(sweep_blocked_on_away(&c, Mode::Attended, Mode::Away, "t").unwrap().is_empty());
    }

    #[test]
    fn only_the_transition_sweeps_not_every_tick_while_away() {
        // Sweeping repeatedly would re-escalate a question the operator is deliberately
        // sitting on, turning away mode into a nag loop.
        let mut c = db();
        blocked_session(&c, "s", "⛔ q");
        assert_eq!(sweep_blocked_on_away(&c, Mode::Away, Mode::Away, "t").unwrap().len(), 0);
    }

    #[test]
    fn returning_to_attended_does_not_sweep() {
        let mut c = db();
        blocked_session(&c, "s", "⛔ q");
        assert!(sweep_blocked_on_away(&c, Mode::Away, Mode::Attended, "t").unwrap().is_empty());
    }

    #[test]
    fn a_session_with_an_open_escalation_is_not_escalated_twice() {
        let mut c = db();
        blocked_session(&c, "s", "⛔ q");
        ask(&c, "s", "already asked", None, "t").unwrap();
        assert!(sweep_blocked_on_away(&c, Mode::Attended, Mode::Away, "t").unwrap().is_empty());
        assert_eq!(open_escalations(&c).unwrap().len(), 1);
    }

    #[test]
    fn a_blocked_session_with_no_question_text_still_reaches_the_operator() {
        // Silence here is the worst outcome: the session waits forever and the operator
        // never learns anyone was waiting.
        let mut c = db();
        blocked_session(&c, "s", "⛔");
        assert_eq!(sweep_blocked_on_away(&c, Mode::Attended, Mode::Away, "t").unwrap().len(), 1);
        assert!(open_escalations(&c).unwrap()[0].2.contains("no question text"));
    }

    #[test]
    fn an_ended_session_is_not_escalated() {
        let mut c = db();
        c.execute("INSERT INTO sessions(name, status, updated_ts, ended_ts) \
                   VALUES('gone','⛔ q','t','t')", []).unwrap();
        assert!(sweep_blocked_on_away(&c, Mode::Attended, Mode::Away, "t").unwrap().is_empty());
    }

    #[test]
    fn resolving_closes_without_an_answer() {
        // 13 of 30 escalations on this machine ended this way: the work moved past the
        // question. It must not look like an answered one.
        let mut c = db();
        let id = ask(&c, "s", "q", None, "t").unwrap();
        assert!(resolve(&c, id, "t").unwrap());
        assert_eq!(escalation_state(&c, id).unwrap().0, "resolved");
        assert!(escalation_state(&c, id).unwrap().1.is_none());
        assert!(open_escalations(&c).unwrap().is_empty());
    }

    #[test]
    fn resolving_an_already_closed_escalation_is_a_no_op() {
        let mut c = db();
        let id = ask(&c, "s", "q", None, "t").unwrap();
        answer(&mut c, id, "the answer", "t").unwrap();
        assert!(!resolve(&c, id, "t").unwrap(), "must not clobber a real answer");
        assert_eq!(escalation_state(&c, id).unwrap().1.as_deref(), Some("the answer"));
    }

    #[test]
    fn operator_messages_are_fifo_and_consumed_once() {
        // Two sessions listening must not both receive the same message and both act.
        let mut c = db();
        record_operator_message(&c, "first", "t").unwrap();
        record_operator_message(&c, "second", "t").unwrap();
        assert_eq!(take_operator_message(&c).unwrap().as_deref(), Some("first"));
        assert_eq!(take_operator_message(&c).unwrap().as_deref(), Some("second"));
        assert!(take_operator_message(&c).unwrap().is_none());
    }

    #[test]
    fn parking_and_saying_are_recorded_for_the_daemon_to_find() {
        let mut c = db();
        park(&c, "s", "waiting on a decision", "t").unwrap();
        enqueue_say(&c, "s", "shipped it", "t").unwrap();
        assert_eq!(open_parked(&c).unwrap().len(), 1);
        assert_eq!(unsent_outbox(&c).unwrap().len(), 1);
    }

    #[test]
    fn escalation_lifecycle() {
        let mut c = db();
        let id = ask(&c, "swift-otter", "deploy to prod?", Some("ship,hold"), &iso(T0)).unwrap();
        assert_eq!(open_escalations(&c).unwrap().len(), 1);
        assert_eq!(unpushed(&c).unwrap().len(), 1);
        mark_pushed(&c, id, Some("42")).unwrap();
        assert!(unpushed(&c).unwrap().is_empty(), "pushed once, not repeatedly");
        assert!(answer(&mut c, id, "ship", &iso(T0 + 60)).unwrap());
        assert!(open_escalations(&c).unwrap().is_empty());
    }

    #[test]
    fn answering_twice_does_not_overwrite_the_first_answer() {
        let mut c = db();
        let id = ask(&c, "s", "q", None, &iso(T0)).unwrap();
        assert!(answer(&mut c, id, "first", &iso(T0)).unwrap());
        assert!(!answer(&mut c, id, "second", &iso(T0)).unwrap());
        let got: String = c
            .query_row("SELECT answer FROM escalations WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(got, "first");
    }
    /// The digest is the one view whose job is to be trusted when it says there is
    /// nothing. Saying "all quiet" while a task waits for the operator's approval is the
    /// only failure here that matters.
    #[test]
    fn a_task_waiting_on_the_operator_breaks_all_quiet() {
        let c = db();
        assert!(digest(&c).contains("all quiet"));
        c.execute(
            "INSERT INTO tasks(id,title,state,priority,scope,origin,created_by,created_ts,\
             updated_ts) VALUES('t-aaaaaa','approve me','review',2,'global','operator',\
             'operator','t','t')", []).unwrap();
        let d = digest(&c);
        assert!(!d.contains("all quiet"), "{d}");
        assert!(d.contains("1 task(s) waiting on you"), "{d}");
    }

    /// A session's own scaffolding is not the operator's problem, and neither is their
    /// own backlog. The digest must stay quiet for both or it stops being read.
    #[test]
    fn a_session_task_and_a_proposed_one_leave_the_digest_quiet() {
        let c = db();
        c.execute(
            "INSERT INTO tasks(id,title,state,priority,scope,origin,created_by,created_ts,\
             updated_ts) VALUES('t-bbbbbb','mine','review',2,'global','session',\
             'swift-otter','t','t')", []).unwrap();
        c.execute(
            "INSERT INTO tasks(id,title,state,priority,scope,origin,created_by,created_ts,\
             updated_ts) VALUES('t-cccccc','an idea','proposed',2,'global','operator',\
             'operator','t','t')", []).unwrap();
        assert!(digest(&c).contains("all quiet"), "{}", digest(&c));
    }

    #[test]
    fn an_answer_reaches_the_session_that_asked() {
        // The whole point of `ask` is that the session went away to do something else,
        // so an answer that only lands in a row is an answer nobody reads.
        let mut c = db();
        let id = ask(&c, "swift-cobra-2", "which option?", None, &iso(T0)).unwrap();
        assert!(answer(&mut c, id, "option b", &iso(T0 + 60)).unwrap());
        let (target, text): (String, String) = c
            .query_row("SELECT target, text FROM messages ORDER BY id DESC LIMIT 1",
                       [], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("answering posts to the bus");
        assert_eq!(target, "@swift-cobra-2", "addressed, or it does not WAKE the listener");
        assert!(text.contains("option b"), "the answer itself must be in it: {text}");
        assert!(text.contains(&format!("#{id}")), "and which question it answers: {text}");
    }

    #[test]
    fn a_second_answer_notifies_nobody() {
        // The guard is on the UPDATE, so a repeat tap must not re-wake the session with
        // an answer it already acted on.
        let mut c = db();
        let id = ask(&c, "swift-cobra-2", "q", None, &iso(T0)).unwrap();
        assert!(answer(&mut c, id, "first", &iso(T0)).unwrap());
        assert!(!answer(&mut c, id, "second", &iso(T0)).unwrap());
        let n: i64 = c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "one answer, one notification");
    }
}
