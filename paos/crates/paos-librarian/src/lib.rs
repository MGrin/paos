//! The review queue: proposals awaiting a human.
//!
//! Split from the CLI on purpose. `paosd` owns every WRITE to `memory_proposals` (see
//! `queue::add` / `queue::set_status`), and the CLI reaches them over the socket or the
//! spool — so there is one writer, and a sandboxed session's approve is consistently
//! DEFERRED rather than half-applied. Reads open the database read-only and may run
//! anywhere.
//!
//! What must not be lost in the port, all of it hard-won:
//!
//! * `superseded` RETIRES a fact, it never deletes one. Every reader filters on it.
//! * Supersede is ATOMIC and takes a LIST, because a tidy merges several facts into one.
//! * Screening is ADVISORY and never auto-rejects — see `screen`.
//! * A split puts every part in ONE proposal row joined by `SPLIT_SEP`. N rows pointing
//!   at one original would delete it on the first approval and strand the rest.

pub mod apply;
pub mod draft;
pub mod dream;
pub mod lessons;
pub mod llm;
pub mod prompts;
pub mod queue;
pub mod screen;
pub mod session;
pub mod upkeep;

pub use queue::{Proposal, SPLIT_SEP};
pub use screen::{is_screened_kind, screen_proposal};
