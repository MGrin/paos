//! Bus domain logic: addressing, wake semantics, message insertion.
//!
//! The addressing and wake rules are **pure functions** here. In the Python they were
//! entangled with SQLite access and `print()`, so the only genuinely unit-testable code
//! in a 2,727-line module was four functions — everything else was an integration test
//! wearing a unit-test hat. These rules are also the ones that have caused the most
//! real-world damage when subtly wrong, so they get tested in isolation.

use rusqlite::{params, Connection};

pub mod push;
pub mod readonly;
pub mod skill;
pub mod wait;

/// The system identity operator-originated messages are posted under. A peer cannot
/// impersonate it: sessions never choose their own handle.
pub const OPERATOR: &str = "operator";

/// Everyone in the room.
pub const ALL: &str = "@all";

/// A message as the delivery logic sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub id: i64,
    pub room: String,
    pub seq: i64,
    pub ts: String,
    pub sender: String,
    /// `@all`, `@handle`, or `@a__b` for multiple recipients.
    pub target: String,
    pub text: String,
    pub urgent: bool,
    /// Machine-generated traffic that is delivered but must not wake anyone.
    pub ambient: bool,
}

/// Is `name` an addressee of `target`?
///
/// `@all` addresses everyone. Otherwise the target is one or more handles joined by
/// `__`. That separator is load-bearing: the fleet used `@alice__bob` for months while
/// the matcher only compared the whole string, so **50 messages addressed nobody and
/// woke nobody** while `send` reported success.
pub fn addressed_to(target: &str, name: &str) -> bool {
    let t = target.trim();
    if t.is_empty() || t == ALL {
        return true;
    }
    t.trim_start_matches('@')
        .split("__")
        .any(|part| part.trim() == name)
}

/// Is this message aimed at one specific set of recipients rather than the whole room?
fn is_broadcast(target: &str) -> bool {
    let t = target.trim();
    t.is_empty() || t == ALL
}

/// Should this message **wake** `name` — i.e. cost them a full turn — or ride along
/// ambiently until their next real wake?
///
/// Waiting is free; only wakes cost tokens. So the lever for efficiency is fewer
/// needless wakes, and this function is that lever.
///
/// Wakes: urgent messages, anything the operator *types*, and anything addressed to you
/// specifically. Does not wake: a plain `@all` peer broadcast, and machine-generated
/// operator traffic flagged `ambient` (the `⚙ operator mode →` banner). Both are still
/// **delivered** — they are batched onto the next real wake — so nothing is lost.
pub fn wakes_listener(m: &Message, name: &str, broadcast_wakes: bool) -> bool {
    if broadcast_wakes {
        return true;
    }
    if m.urgent {
        return true;
    }
    if !is_broadcast(&m.target) {
        // Addressed to someone specific — but only wake if that someone is us.
        return addressed_to(&m.target, name);
    }
    if m.ambient {
        return false;
    }
    m.sender == OPERATOR
}

/// Append a message to a room and return its `(id, seq)`.
///
/// `seq` is allocated inside an IMMEDIATE transaction. The Python allocated it with
/// `SELECT MAX(seq)+1` then INSERT, relying on *callers* to have opened the
/// transaction — a convention, not an invariant, and a live `UNIQUE(room, seq)` race
/// once the daemon and N sessions were all writing.
pub fn post(
    conn: &mut Connection,
    room: &str,
    sender: &str,
    target: &str,
    text: &str,
    ts: &str,
    urgent: bool,
    ambient: bool,
) -> rusqlite::Result<(i64, i64)> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT OR IGNORE INTO rooms(room, created_ts) VALUES(?1, ?2)",
        params![room, ts],
    )?;
    // POSTING JOINS, and it belongs HERE rather than in the request handler because there
    // are two write paths and the handler is the one almost nobody takes: a sandboxed
    // session cannot reach the unix socket, so its sends are SPOOLED and replayed through
    // `apply_bus_op`, which calls this function directly. Fixing only the handler was
    // tested green by a unit test and changed nothing in production — the live check
    // afterwards still showed zero members.
    //
    // Why join at all: membership is what delivery uses, so a session could talk into a
    // room forever while being unreachable in it. Measured on the live store, `ad-hocs`
    // had a Telegram topic, several sessions posting into it, and ZERO members — every
    // message the operator typed there landed in an empty room. Their words: "sessions
    // write to the ad-hocs room but are not reachable through it".
    //
    // Never `operator`: the human is not a fleet session, and putting them on the roster
    // would make them a candidate for peer-directed traffic.
    //
    // LIVE senders only. Spool entries replay with their ORIGINAL timestamp, sometimes
    // days later, so an unguarded join resurrects the membership of a session that has
    // since ended — teardown deletes the row and the drain puts it straight back. Nine
    // ghosts appeared in `lobby` that way, each with `joined_ts` equal to its session's
    // `ended_ts` to the second.
    if sender != "operator" {
        let live: bool = tx
            .query_row(
                "SELECT 1 FROM sessions WHERE name=?1 AND ended_ts IS NULL",
                [sender],
                |_| Ok(()),
            )
            .is_ok();
        if live {
            tx.execute(
                "INSERT OR IGNORE INTO members(room, name, joined_ts, last_seen) \
                 VALUES(?1, ?2, ?3, ?3)",
                params![room, sender, ts],
            )?;
        }
    }
    let seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE room = ?1",
        [room],
        |r| r.get(0),
    )?;
    tx.execute(
        "INSERT INTO messages(room, seq, ts, sender, target, text, urgent, ambient) \
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![room, seq, ts, sender, target, text, urgent as i64, ambient as i64],
    )?;
    let id = tx.last_insert_rowid();
    tx.commit()?;
    Ok((id, seq))
}

/// Messages in `room` newer than `after_seq`, oldest first.
pub fn unread(conn: &Connection, room: &str, after_seq: i64) -> rusqlite::Result<Vec<Message>> {
    let mut stmt = conn.prepare(
        "SELECT id, room, seq, ts, sender, target, text, \
                COALESCE(urgent,0), COALESCE(ambient,0) \
         FROM messages WHERE room = ?1 AND seq > ?2 ORDER BY seq",
    )?;
    let rows = stmt.query_map(params![room, after_seq], |r| {
        Ok(Message {
            id: r.get(0)?,
            room: r.get(1)?,
            seq: r.get(2)?,
            ts: r.get(3)?,
            sender: r.get(4)?,
            target: r.get(5)?,
            text: r.get(6)?,
            urgent: r.get::<_, i64>(7)? != 0,
            ambient: r.get::<_, i64>(8)? != 0,
        })
    })?;
    rows.collect()
}

/// Rooms `name` has joined — **`lobby` always first, and always present.**
///
/// Two rules that look cosmetic and are not:
///
/// 1. `lobby` is prepended whether or not a `members` row exists for it. This list is
///    what `wait-joined` listens on, so a session whose lobby membership was pruned (by a
///    room sweep, an orphan purge, or a crash between join and heartbeat) would otherwise
///    stop hearing lobby traffic — including operator broadcasts — while everything still
///    reported healthy. That is the fleet's signature silent failure.
/// 2. The remaining rooms keep the table's natural order rather than being sorted, so the
///    output matches the Python byte for byte during the cutover.
pub fn joined_rooms(conn: &Connection, name: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT DISTINCT room FROM members WHERE name = ?1")?;
    let rows = stmt.query_map([name], |r| r.get::<_, String>(0))?;
    let mut out = vec!["lobby".to_string()];
    for r in rows {
        let r = r?;
        if r != "lobby" && !out.contains(&r) {
            out.push(r);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(sender: &str, target: &str) -> Message {
        Message {
            id: 1,
            room: "lobby".into(),
            seq: 1,
            ts: "t".into(),
            sender: sender.into(),
            target: target.into(),
            text: "x".into(),
            urgent: false,
            ambient: false,
        }
    }

    // ---- addressing ----

    #[test]
    fn all_addresses_everyone() {
        assert!(addressed_to("@all", "swift-otter"));
        assert!(addressed_to("", "swift-otter"));
    }

    #[test]
    fn exact_handle_matches_only_that_handle() {
        assert!(addressed_to("@swift-otter", "swift-otter"));
        assert!(!addressed_to("@swift-otter", "quiet-bison"));
    }

    #[test]
    fn multi_recipient_double_underscore_addresses_each() {
        // REGRESSION: the matcher used to compare the whole string, so the `@a__b` form
        // the fleet had adopted addressed NOBODY. 50 messages woke no one while `send`
        // reported success.
        assert!(addressed_to("@alice__bob", "alice"));
        assert!(addressed_to("@alice__bob", "bob"));
        assert!(!addressed_to("@alice__bob", "carol"));
    }

    #[test]
    fn a_handle_is_not_matched_by_a_prefix() {
        assert!(!addressed_to("@swift", "swift-otter"));
        assert!(!addressed_to("@swift-otter-2", "swift-otter"));
    }

    // ---- wake semantics ----

    #[test]
    fn peer_broadcast_does_not_wake() {
        assert!(!wakes_listener(&msg("a-peer", "@all"), "me", false));
    }

    #[test]
    fn addressed_message_wakes_the_addressee_only() {
        assert!(wakes_listener(&msg("a-peer", "@me"), "me", false));
        assert!(!wakes_listener(&msg("a-peer", "@you"), "me", false));
    }

    #[test]
    fn urgent_always_wakes_even_as_a_broadcast() {
        let mut m = msg("a-peer", "@all");
        m.urgent = true;
        assert!(wakes_listener(&m, "me", false));
    }

    #[test]
    fn operator_broadcast_wakes_but_ambient_operator_traffic_does_not() {
        // Anything the operator TYPES must reach a session immediately — that is the
        // whole point of the channel. The machine-generated mode banner must not:
        // waking every session for it cost one full turn each per /away toggle
        // (9 lobby members, ~17.6M tokens of context between them).
        assert!(wakes_listener(&msg(OPERATOR, "@all"), "me", false));
        let mut banner = msg(OPERATOR, "@all");
        banner.ambient = true;
        assert!(!wakes_listener(&banner, "me", false));
    }

    #[test]
    fn ambient_still_wakes_when_addressed_to_you_specifically() {
        let mut m = msg(OPERATOR, "@me");
        m.ambient = true;
        assert!(wakes_listener(&m, "me", false));
    }

    #[test]
    fn ambient_still_wakes_when_urgent() {
        let mut m = msg(OPERATOR, "@all");
        m.ambient = true;
        m.urgent = true;
        assert!(wakes_listener(&m, "me", false));
    }

    #[test]
    fn escape_hatch_restores_wake_everything() {
        assert!(wakes_listener(&msg("a-peer", "@all"), "me", true));
    }

    // ---- persistence ----

    fn db() -> Connection {
        paos_store::open_in_memory().unwrap()
    }

    #[test]
    fn post_allocates_sequential_seq_per_room() {
        let mut c = db();
        assert_eq!(post(&mut c, "lobby", "a", "@all", "1", "t", false, false).unwrap().1, 1);
        assert_eq!(post(&mut c, "lobby", "a", "@all", "2", "t", false, false).unwrap().1, 2);
        // seq is per-room, not global
        assert_eq!(post(&mut c, "other", "a", "@all", "1", "t", false, false).unwrap().1, 1);
    }

    #[test]
    fn post_creates_the_room_if_absent() {
        let mut c = db();
        post(&mut c, "brand-new", "a", "@all", "hi", "t", false, false).unwrap();
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM rooms WHERE room='brand-new'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn unread_returns_only_messages_after_the_cursor() {
        let mut c = db();
        for i in 1..=3 {
            post(&mut c, "lobby", "a", "@all", &format!("m{i}"), "t", false, false).unwrap();
        }
        let got = unread(&c, "lobby", 1).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].text, "m2");
        assert_eq!(got[1].text, "m3");
    }

    #[test]
    fn unread_round_trips_the_urgent_and_ambient_flags() {
        // These two booleans decide whether a peer is woken; losing them in transit
        // would silently change delivery semantics.
        let mut c = db();
        post(&mut c, "lobby", OPERATOR, "@all", "banner", "t", false, true).unwrap();
        post(&mut c, "lobby", "a", "@all", "wake", "t", true, false).unwrap();
        let got = unread(&c, "lobby", 0).unwrap();
        assert!(got[0].ambient && !got[0].urgent);
        assert!(got[1].urgent && !got[1].ambient);
    }

    #[test]
    fn joined_rooms_lists_only_this_members_rooms_with_lobby_first() {
        let c = db();
        c.execute("INSERT INTO members(room,name) VALUES('lobby','me')", []).unwrap();
        c.execute("INSERT INTO members(room,name) VALUES('adhoc','me')", []).unwrap();
        c.execute("INSERT INTO members(room,name) VALUES('lobby','you')", []).unwrap();
        assert_eq!(joined_rooms(&c, "me").unwrap(), vec!["lobby", "adhoc"]);
    }

    #[test]
    fn lobby_is_listened_to_even_with_no_membership_row() {
        // THE SILENT-DEAFNESS CASE. This list is what `wait-joined` listens on. A session
        // whose lobby row was pruned — by a room sweep, an orphan purge, or a crash
        // between join and heartbeat — would stop hearing lobby traffic AND operator
        // broadcasts, with `who` still showing it live and nothing erroring anywhere.
        //
        // Not hypothetical: witty-bison-2 had exactly one membership row (`ad-hocs`) and
        // no lobby row while this was written, and the Python was still listening on
        // lobby because it prepends unconditionally.
        let c = db();
        c.execute("INSERT INTO members(room,name) VALUES('ad-hocs','me')", []).unwrap();
        assert_eq!(joined_rooms(&c, "me").unwrap(), vec!["lobby", "ad-hocs"]);

        // ...and a session with no rows at all still hears the lobby.
        assert_eq!(joined_rooms(&c, "brand-new").unwrap(), vec!["lobby"]);
    }

    #[test]
    fn joined_rooms_does_not_duplicate_lobby() {
        let c = db();
        c.execute("INSERT INTO members(room,name) VALUES('lobby','me')", []).unwrap();
        let got = joined_rooms(&c, "me").unwrap();
        assert_eq!(got, vec!["lobby"], "prepending must not double it up: {got:?}");
    }
}
