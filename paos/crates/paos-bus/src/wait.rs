//! The wake loop: what a session re-arms at the end of every turn.
//!
//! This is the most dangerous code in the bus. `paos bus wait-joined` is the listener
//! every session on this machine keeps armed, and `paos bus reachable` is the self-heal
//! that checks it. If either regresses, nobody gets messages **and nothing errors** —
//! the fleet just goes quiet and every session believes it is fine.
//!
//! The decisions live here as pure functions so they can be tested without a clock, a
//! database or a signal handler. In the Python they were interleaved with `time.sleep`,
//! `print` and SQLite, so the only way to test them was to run the loop.

use crate::Message;

/// `listen` timed out with nothing to deliver — the caller should re-arm.
pub const RE_ARM_EXIT: i32 = 75;
/// Another listener already holds this identity's lock.
pub const ALREADY_LISTENING_EXIT: i32 = 3;
/// Kept for callers that still reference it. `wait` itself never returns it: DND is
/// urgent-permeable now, so a DND'd session stays armed instead of stopping.
pub const DND_STOP_EXIT: i32 = 4;
/// Every listened room is closed.
pub const ROOM_CLOSED_EXIT: i32 = 5;
/// Signalled to stop, or orphaned from the harness. The session re-arms a fresh one.
pub const TEARDOWN_EXIT: i32 = 6;

/// Re-arm windows, in seconds, then `WAIT_STEADY` forever.
///
/// The ramp exists so a session that was just active re-checks quickly, while a
/// long-idle one settles into a cheap 30-minute cycle. Waiting is free; only a wake
/// costs a turn.
pub const WAIT_SCHEDULE: [u64; 5] = [90, 180, 360, 720, 1440];
pub const WAIT_STEADY: u64 = 1800;

/// Most messages delivered on a single wake. Urgent and operator messages are never
/// dropped, however far behind the cursor is.
pub const BACKLOG_MAX_DELIVER: usize = 25;

/// The window for iteration `i`: the schedule, then the steady state.
pub fn window_for(i: usize, schedule: &[u64], steady: u64) -> u64 {
    schedule.get(i).copied().unwrap_or(steady)
}

/// Messages `name` has not yet read and is entitled to see.
///
/// Skips: anything at or before the cursor, anything this session sent, and — under DND
/// — anything that is neither urgent nor from the operator. The operator channel always
/// penetrates DND because it is the human.
pub fn unread_for(msgs: &[Message], cursor: i64, name: &str, urgent_only: bool) -> Vec<Message> {
    msgs.iter()
        .filter(|m| m.seq > cursor)
        .filter(|m| m.sender != name)
        .filter(|m| !urgent_only || m.urgent || m.sender == crate::OPERATOR)
        .filter(|m| crate::addressed_to(&m.target, name))
        .cloned()
        .collect()
}

/// Keep the newest `limit`, returning `(delivered, skipped)`.
///
/// Urgent and operator messages are exempt: a session that fell far behind must still see
/// the human's instruction and the explicit wakes, even if ordinary chatter is dropped.
pub fn cap_backlog(msgs: &[Message], limit: usize) -> (Vec<Message>, usize) {
    if limit == 0 || msgs.len() <= limit {
        return (msgs.to_vec(), 0);
    }
    let must: Vec<Message> = msgs.iter()
        .filter(|m| m.urgent || m.sender == crate::OPERATOR).cloned().collect();
    let rest: Vec<Message> = msgs.iter()
        .filter(|m| !(m.urgent || m.sender == crate::OPERATOR)).cloned().collect();
    let mut keep = must;
    if limit > keep.len() {
        let take = limit - keep.len();
        let start = rest.len().saturating_sub(take);
        keep.extend_from_slice(&rest[start..]);
    }
    keep.sort_by_key(|m| m.seq);
    let skipped = msgs.len() - keep.len();
    (keep, skipped)
}

/// Does this batch justify spending a turn?
///
/// Delivery and waking are different questions. A plain `@all` peer broadcast is
/// delivered but does not wake: a wake is a full LLM turn, and a chatty fleet used to
/// spend one per session per broadcast. On a real wake ALL unread is handed over, so the
/// ambient traffic rides the turn that was going to be spent anyway and nothing is lost.
pub fn any_wakes(msgs: &[Message], name: &str, broadcast_wakes: bool) -> bool {
    msgs.iter().any(|m| crate::wakes_listener(m, name, broadcast_wakes))
}

/// Should the loop stop because every room it listens to is closed?
///
/// Only when there IS at least one target: an empty target list means "not joined
/// anywhere yet", which is a reason to keep waiting, not to exit. Treating it as
/// all-closed would make a freshly-started session exit immediately and go deaf.
pub fn all_rooms_closed(targets: &[String], closed: &dyn Fn(&str) -> bool) -> bool {
    !targets.is_empty() && targets.iter().all(|r| closed(r))
}

/// Is this listener orphaned from the harness?
///
/// A tracked listener's parent is the harness shell. If it is reparented to init
/// (`ppid == 1`) the harness can no longer wake the session from it, so the lock must be
/// freed for a fresh, tracked listener. `start_ppid` of 0/1/unknown disables the check —
/// otherwise a listener legitimately started outside a harness would exit at once.
pub fn is_orphaned(start_ppid: Option<u32>, current_ppid: u32) -> bool {
    match start_ppid {
        Some(p) if p != 0 && p != 1 => current_ppid == 1,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(seq: i64, sender: &str, target: &str) -> Message {
        Message {
            id: seq, room: "lobby".into(), seq, ts: "t".into(),
            sender: sender.into(), target: target.into(), text: format!("m{seq}"),
            urgent: false, ambient: false,
        }
    }
    fn urgent(seq: i64, sender: &str, target: &str) -> Message {
        let mut x = m(seq, sender, target); x.urgent = true; x
    }

    // ---- the re-arm ramp ----

    #[test]
    fn the_window_ramps_then_settles() {
        let s = WAIT_SCHEDULE;
        assert_eq!(window_for(0, &s, WAIT_STEADY), 90);
        assert_eq!(window_for(4, &s, WAIT_STEADY), 1440);
        assert_eq!(window_for(5, &s, WAIT_STEADY), 1800, "past the ramp it is steady");
        assert_eq!(window_for(500, &s, WAIT_STEADY), 1800);
    }

    #[test]
    fn the_steady_window_stays_under_the_reap_threshold() {
        // The listener heartbeats once per window. If the window ever exceeded the
        // reaper's 90-minute threshold, a live listening session would be archived as
        // dead and have its room memberships cascaded — i.e. made deaf by the reaper.
        assert!(WAIT_STEADY < 5400, "steady window must stay under REAP_THRESHOLD_S");
    }

    // ---- what counts as unread ----

    #[test]
    fn unread_skips_read_own_and_unaddressed_messages() {
        let msgs = vec![
            m(1, "peer", "@me"),      // before cursor
            m(2, "me", "@all"),       // our own
            m(3, "peer", "@someone"), // not for us
            m(4, "peer", "@me"),      // ours
        ];
        let got = unread_for(&msgs, 1, "me", false);
        assert_eq!(got.iter().map(|x| x.seq).collect::<Vec<_>>(), vec![4]);
    }

    #[test]
    fn under_dnd_only_urgent_and_the_operator_get_through() {
        // Phone-DND semantics: "not for chatter, but a wake still reaches me". The
        // operator is the human — that channel must never be filtered.
        let msgs = vec![
            m(1, "peer", "@me"),
            urgent(2, "peer", "@me"),
            m(3, crate::OPERATOR, "@me"),
        ];
        let got = unread_for(&msgs, 0, "me", true);
        assert_eq!(got.iter().map(|x| x.seq).collect::<Vec<_>>(), vec![2, 3]);
        // ...and with DND off, all three are unread.
        assert_eq!(unread_for(&msgs, 0, "me", false).len(), 3);
    }

    // ---- waking vs delivering ----

    #[test]
    fn a_peer_broadcast_is_delivered_but_does_not_wake() {
        let msgs = vec![m(1, "peer", "@all")];
        assert_eq!(unread_for(&msgs, 0, "me", false).len(), 1, "delivered");
        assert!(!any_wakes(&msgs, "me", false), "but does not cost a turn");
    }

    #[test]
    fn addressed_urgent_and_operator_all_wake() {
        assert!(any_wakes(&[m(1, "peer", "@me")], "me", false));
        assert!(any_wakes(&[urgent(1, "peer", "@all")], "me", false));
        assert!(any_wakes(&[m(1, crate::OPERATOR, "@all")], "me", false));
    }

    #[test]
    fn one_waking_message_lifts_a_whole_ambient_batch() {
        // The catch-up rides the turn that was going to be spent anyway.
        let msgs = vec![m(1, "peer", "@all"), m(2, "peer", "@all"), m(3, "peer", "@me")];
        assert!(any_wakes(&msgs, "me", false));
    }

    #[test]
    fn the_escape_hatch_makes_every_broadcast_wake() {
        assert!(any_wakes(&[m(1, "peer", "@all")], "me", true));
    }

    // ---- backlog cap ----

    #[test]
    fn a_backlog_keeps_the_newest_and_reports_the_rest() {
        let msgs: Vec<Message> = (1..=30).map(|i| m(i, "peer", "@me")).collect();
        let (keep, skipped) = cap_backlog(&msgs, 25);
        assert_eq!(keep.len(), 25);
        assert_eq!(skipped, 5);
        assert_eq!(keep.first().unwrap().seq, 6, "the newest 25");
        assert_eq!(keep.last().unwrap().seq, 30);
    }

    #[test]
    fn urgent_and_operator_messages_survive_any_backlog() {
        // A session far behind must still see the human's instruction. Dropping it to
        // make room for ordinary chatter is the worst possible trade.
        let mut msgs: Vec<Message> = (1..=40).map(|i| m(i, "peer", "@me")).collect();
        msgs[0] = urgent(1, "peer", "@me");
        msgs[1] = m(2, crate::OPERATOR, "@me");
        let (keep, _) = cap_backlog(&msgs, 5);
        assert!(keep.iter().any(|x| x.seq == 1), "the urgent one is kept");
        assert!(keep.iter().any(|x| x.seq == 2), "the operator one is kept");
        assert!(keep.windows(2).all(|w| w[0].seq < w[1].seq), "and order is preserved");
    }

    #[test]
    fn a_batch_within_the_cap_is_untouched() {
        let msgs: Vec<Message> = (1..=5).map(|i| m(i, "peer", "@me")).collect();
        let (keep, skipped) = cap_backlog(&msgs, 25);
        assert_eq!(keep.len(), 5);
        assert_eq!(skipped, 0);
    }

    #[test]
    fn more_exempt_messages_than_the_cap_still_all_arrive() {
        // Never drop a human's message to satisfy a limit.
        let msgs: Vec<Message> = (1..=10).map(|i| urgent(i, "peer", "@me")).collect();
        let (keep, skipped) = cap_backlog(&msgs, 3);
        assert_eq!(keep.len(), 10);
        assert_eq!(skipped, 0);
    }

    // ---- stop conditions ----

    #[test]
    fn no_rooms_is_a_reason_to_keep_waiting_not_to_exit() {
        // REGRESSION GUARD: treating "joined nowhere" as "all closed" would make a
        // freshly-started session exit immediately and go deaf without erroring.
        assert!(!all_rooms_closed(&[], &|_| true));
    }

    #[test]
    fn all_closed_stops_but_one_open_room_keeps_listening() {
        let rooms = vec!["a".to_string(), "b".to_string()];
        assert!(all_rooms_closed(&rooms, &|_| true));
        assert!(!all_rooms_closed(&rooms, &|r| r == "a"));
    }

    #[test]
    fn orphan_detection_only_fires_for_a_harness_tracked_listener() {
        assert!(is_orphaned(Some(4242), 1), "reparented to init");
        assert!(!is_orphaned(Some(4242), 4242), "still tracked");
        // Started with no usable parent: the check must not fire, or a listener armed
        // outside a harness would exit on its first iteration.
        assert!(!is_orphaned(None, 1));
        assert!(!is_orphaned(Some(0), 1));
        assert!(!is_orphaned(Some(1), 1));
    }

    #[test]
    fn the_exit_codes_are_distinct_so_a_wake_can_be_classified() {
        // A woken session branches on the code, not on prose. Two codes colliding would
        // make "already listening" indistinguishable from "room closed".
        let codes = [0, ALREADY_LISTENING_EXIT, DND_STOP_EXIT, ROOM_CLOSED_EXIT,
                     TEARDOWN_EXIT, RE_ARM_EXIT];
        let mut seen = std::collections::HashSet::new();
        for c in codes {
            assert!(seen.insert(c), "exit code {c} is used twice");
        }
        // And none may collide with 128+N signal codes.
        assert!(codes.iter().all(|c| *c < 128));
    }
}
