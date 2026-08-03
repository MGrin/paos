//! The fleet's shared work queue.
//!
//! Domain only — the state machine, the atomic claim, and the queries. No argv, no HTTP,
//! no sockets. That is what lets the concurrency test open two real connections to one
//! temp database without booting anything, which is the only honest way to test a claim
//! that two sessions can race.
//!
//! The single rule this crate exists to enforce: **ownership is `claimed_by IS NULL`,
//! and `blocked` is a query.** Both were tempting to store as flags, and a stored flag
//! needs something to maintain it. In this system nothing would — that is exactly how
//! `bus blocked` came to be documented and unused across 1,031 sessions.

pub mod model;
pub mod query;
pub mod store;
