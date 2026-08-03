//! The push registry — what replaces the 1 Hz poll.
//!
//! The Python listener re-queried `SELECT MAX(id) FROM messages WHERE room=?` once per
//! second, per room, per session, forever. Measured at idle with 5 sessions: **92,900
//! SQLite queries/hour, 62,400 connection opens/hour, 7 timer wakeups/second** — to
//! carry a peak of 971 messages/day. A poll-to-message ratio near 100,000:1.
//!
//! Here a listener registers once and then blocks on a channel. Delivery costs exactly
//! one send at post time and nothing at all while idle.

use crate::{wakes_listener, Message};
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;

/// A registered listener: who it is, which rooms it watches, and how it is reached.
struct Listener {
    name: String,
    rooms: Vec<String>,
    /// Do Not Disturb — urgent-permeable, not deaf. Under DND normal chatter is
    /// filtered but `paos bus wake` and the operator still get through, which is the
    /// phone-DND semantics the fleet relies on.
    urgent_only: bool,
    tx: Sender<Message>,
}

/// Registration handle. Dropping it deregisters, so a hung-up client cannot leak a slot.
pub struct Subscription {
    pub id: u64,
    pub rx: Receiver<Message>,
}

#[derive(Default)]
pub struct Registry {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    listeners: HashMap<u64, Listener>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a listener and get the receiving end to block on.
    pub fn subscribe(&self, name: &str, rooms: Vec<String>, urgent_only: bool) -> Subscription {
        let (tx, rx) = channel();
        let mut inner = self.lock();
        inner.next_id += 1;
        let id = inner.next_id;
        inner.listeners.insert(
            id,
            Listener { name: name.to_string(), rooms, urgent_only, tx },
        );
        Subscription { id, rx }
    }

    pub fn unsubscribe(&self, id: u64) {
        self.lock().listeners.remove(&id);
    }

    /// Is any listener currently attached for `name`?
    ///
    /// This replaces the whole flock + process-table liveness dance: the daemon holds
    /// the connection, so reachability is a fact rather than an inference.
    pub fn has_listener(&self, name: &str) -> bool {
        self.lock().listeners.values().any(|l| l.name == name)
    }

    pub fn len(&self) -> usize {
        self.lock().listeners.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Deliver `m` to every listener it should wake. Returns how many were woken.
    ///
    /// Called once, synchronously, after the post commits. A listener whose receiver has
    /// hung up is dropped here rather than accumulating — the Python equivalent leaked
    /// orphaned listeners that then had to be reaped by a process-table scan the sandbox
    /// could not even see.
    pub fn publish(&self, m: &Message, broadcast_wakes: bool) -> usize {
        let mut inner = self.lock();
        let mut dead = Vec::new();
        let mut woken = 0;
        for (id, l) in inner.listeners.iter() {
            if !l.rooms.iter().any(|r| r == &m.room) {
                continue;
            }
            if !wakes_listener(m, &l.name, broadcast_wakes) {
                continue; // delivered later, batched — never a wake
            }
            if l.urgent_only && !(m.urgent || m.sender == crate::OPERATOR) {
                continue; // DND: urgent-permeable, not deaf
            }
            if l.tx.send(m.clone()).is_err() {
                dead.push(*id);
            } else {
                woken += 1;
            }
        }
        for id in dead {
            inner.listeners.remove(&id);
        }
        woken
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panicking handler must not wedge the whole bus.
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::OPERATOR;

    fn msg(room: &str, sender: &str, target: &str) -> Message {
        Message {
            id: 1,
            room: room.into(),
            seq: 1,
            ts: "t".into(),
            sender: sender.into(),
            target: target.into(),
            text: "x".into(),
            urgent: false,
            ambient: false,
        }
    }

    #[test]
    fn addressed_message_reaches_the_listener() {
        let r = Registry::new();
        let s = r.subscribe("me", vec!["lobby".into()], false);
        assert_eq!(r.publish(&msg("lobby", "peer", "@me"), false), 1);
        assert_eq!(s.rx.try_recv().unwrap().text, "x");
    }

    #[test]
    fn a_room_you_are_not_in_never_reaches_you() {
        let r = Registry::new();
        let s = r.subscribe("me", vec!["lobby".into()], false);
        assert_eq!(r.publish(&msg("other-room", "peer", "@me"), false), 0);
        assert!(s.rx.try_recv().is_err());
    }

    #[test]
    fn peer_broadcast_wakes_nobody() {
        // The single biggest token saving in the mesh.
        let r = Registry::new();
        let a = r.subscribe("a", vec!["lobby".into()], false);
        let b = r.subscribe("b", vec!["lobby".into()], false);
        assert_eq!(r.publish(&msg("lobby", "peer", "@all"), false), 0);
        assert!(a.rx.try_recv().is_err());
        assert!(b.rx.try_recv().is_err());
    }

    #[test]
    fn operator_message_wakes_everyone_in_the_room() {
        let r = Registry::new();
        let a = r.subscribe("a", vec!["lobby".into()], false);
        let b = r.subscribe("b", vec!["lobby".into()], false);
        assert_eq!(r.publish(&msg("lobby", OPERATOR, "@all"), false), 2);
        assert!(a.rx.try_recv().is_ok());
        assert!(b.rx.try_recv().is_ok());
    }

    #[test]
    fn dnd_filters_chatter_but_not_urgent_or_operator() {
        let r = Registry::new();
        let s = r.subscribe("me", vec!["lobby".into()], true);
        assert_eq!(r.publish(&msg("lobby", "peer", "@me"), false), 0, "chatter filtered");

        let mut urgent = msg("lobby", "peer", "@me");
        urgent.urgent = true;
        assert_eq!(r.publish(&urgent, false), 1, "wake must penetrate DND");

        assert_eq!(r.publish(&msg("lobby", OPERATOR, "@me"), false), 1, "operator too");
        assert_eq!(s.rx.try_recv().unwrap().urgent, true);
    }

    #[test]
    fn ambient_operator_banner_does_not_wake_the_fleet() {
        let r = Registry::new();
        let _a = r.subscribe("a", vec!["lobby".into()], false);
        let _b = r.subscribe("b", vec!["lobby".into()], false);
        let mut banner = msg("lobby", OPERATOR, "@all");
        banner.ambient = true;
        assert_eq!(r.publish(&banner, false), 0);
    }

    #[test]
    fn hung_up_listener_is_reaped_on_publish() {
        // Orphaned listeners used to accumulate and had to be found via the process
        // table — which returns nothing inside the sandbox.
        let r = Registry::new();
        let s = r.subscribe("me", vec!["lobby".into()], false);
        assert_eq!(r.len(), 1);
        drop(s);
        assert_eq!(r.publish(&msg("lobby", "peer", "@me"), false), 0);
        assert!(r.is_empty(), "dead listener must be removed");
    }

    #[test]
    fn unsubscribe_removes_the_slot() {
        let r = Registry::new();
        let s = r.subscribe("me", vec!["lobby".into()], false);
        r.unsubscribe(s.id);
        assert!(r.is_empty());
    }

    #[test]
    fn two_sessions_same_handle_both_receive() {
        // A re-armed listener may briefly overlap with the one it replaces; neither
        // should be starved.
        let r = Registry::new();
        let a = r.subscribe("me", vec!["lobby".into()], false);
        let b = r.subscribe("me", vec!["lobby".into()], false);
        assert_eq!(r.publish(&msg("lobby", "peer", "@me"), false), 2);
        assert!(a.rx.try_recv().is_ok());
        assert!(b.rx.try_recv().is_ok());
    }

    #[test]
    fn has_listener_is_a_fact_not_an_inference() {
        let r = Registry::new();
        assert!(!r.has_listener("me"));
        let s = r.subscribe("me", vec!["lobby".into()], false);
        assert!(r.has_listener("me"));
        assert!(!r.has_listener("someone-else"));
        r.unsubscribe(s.id);
        assert!(!r.has_listener("me"));
    }
}