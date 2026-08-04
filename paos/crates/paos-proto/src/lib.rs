//! Wire protocol between the `paos` CLI and the `paosd` daemon.
//!
//! Length-prefixed JSON over a unix socket: a 4-byte big-endian length, then that many
//! bytes of UTF-8 JSON. Chosen over gRPC/protobuf deliberately — it is readable on the
//! wire with `nc`, needs no schema compiler, and at single-user scale the encoding cost
//! is irrelevant next to the 12 µs round-trip.

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

/// Refuse absurd frames rather than allocating whatever a caller claims. Nothing the
/// CLI sends is remotely this large; a bigger frame means a bug or a wedged stream.
pub const MAX_FRAME: u32 = 8 * 1024 * 1024;

/// A request from the CLI. One variant per command; `serde` tags it by `cmd`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    /// Liveness probe. Never touches the database.
    Ping,
    /// The daemon's own version, for install/upgrade checks.
    Version,
    /// This session's bus handle.
    Whoami { session_id: Option<String> },
    /// Post a message to a room.
    Send {
        room: String,
        sender: String,
        #[serde(default = "default_target")]
        target: String,
        text: String,
        #[serde(default)]
        urgent: bool,
        #[serde(default)]
        ambient: bool,
    },
    /// Register as a listener and block until something wakes us.
    ///
    /// This is the request that replaces polling: the connection stays open and the
    /// daemon writes a frame when — and only when — a message warrants a wake.
    Listen {
        name: String,
        rooms: Vec<String>,
        #[serde(default)]
        urgent_only: bool,
    },
    /// Bind a handle to a Claude session id and mark it online (SessionStart hook).
    SessionStart { session_id: String, name: String, pid: Option<i64> },
    /// Advance last_seen (Stop hook, every assistant turn).
    Heartbeat {
        session_id: String,
        /// The session's process id, RE-ASSERTED on every turn.
        ///
        /// The pid is the reaper's primary confirmed-death signal, so a row carrying a
        /// stale one trades a fast, certain answer for a staleness timeout — and fails
        /// silently, in the direction this system already fails. The Python writes it
        /// every heartbeat; dropping it here would have been a quiet regression.
        #[serde(default)]
        pid: Option<i64>,
    },
    /// Archive the session and drop its memberships (SessionEnd hook).
    SessionEnd { session_id: String },
    /// The live fleet roster.
    Who,
    /// Verify — and repair — reachability. The end-of-turn reflex.
    Reachable { name: String },
    /// Store a durable fact. `tier` is mandatory — there is deliberately no default.
    ///
    /// `dataset` overrides tier/origin derivation for callers that already know exactly
    /// where a fact belongs — specifically the review queue, whose proposals record
    /// their dataset at draft time and must land in that one, not in whatever the
    /// approving session's cwd happens to derive.
    Remember {
        tier: String,
        origin: Option<String>,
        text: String,
        #[serde(default)]
        dataset: Option<String>,
    },
    /// Search project + org + global for this origin.
    /// Search project + org + global for this origin — or ONE named dataset.
    ///
    /// `dataset` exists because a dataset no repo maps to (a personal tier, say) is
    /// otherwise reachable only by `show`, which DUMPS it. Dumping is not searching, so
    /// without this a "scoped" fact is really a buried one — and nothing errors to say so.
    Recall {
        origin: Option<String>,
        query: String,
        #[serde(default = "default_k")]
        top_k: usize,
        #[serde(default)]
        dataset: Option<String>,
        /// Search EVERY dataset, not the tiers the cwd implies.
        ///
        /// Outside a git repo recall degrades to the global brain alone — 186 facts of
        /// 1,260 on this machine — and said nothing about it, so it read as a search of
        /// everything that simply found little. This is the way to ask for everything on
        /// purpose.
        #[serde(default)]
        all_scopes: bool,
},
    Forget { id: String },
    /// Retire facts without replacing them: they stop being recalled but stay on disk.
    ///
    /// Distinct from `Forget`, which deletes. A retirement is a judgement that a fact has
    /// stopped being useful, and judgements about a human's memory should be reversible
    /// with an UPDATE rather than a restore from backup.
    Retire { ids: Vec<String> },
    /// Compute second-stage vectors for facts that have none. Resumable by design: it
    /// indexes at most `limit` per call, so a 1,600-fact store is caught up in batches
    /// rather than holding the single writer for minutes.
    RerankIndex { limit: usize },
    /// Store `text` and retire `old_id` in its favour, keeping the original auditable.
    ///
    /// `memories.superseded` and `paos_memory::supersede` both existed, every reader
    /// already filtered on the column, and the docstring promised the history "stays
    /// auditable" — but no verb ever set it, so it held 0 rows across 1051 facts. The
    /// Python did store-then-hard-DELETE instead, which is exactly what the column was
    /// added to avoid.
    ///
    /// Store and retire together, in one request under one lock: the caller cannot mint
    /// the new id itself without reimplementing the id hash in a second language, and
    /// splitting it into two calls means a crash between them either loses the original
    /// or retires it with nothing in its place.
    /// `old_ids` is a list because a `tidy` merges several facts into one, and a merge
    /// that retires only the first source leaves the rest live and recallable — the
    /// duplicate the merge existed to remove.
    Supersede {
        old_ids: Vec<String>,
        tier: String,
        origin: Option<String>,
        text: String,
        #[serde(default)]
        dataset: Option<String>,
    },
    /// Append to the cross-facet activity journal.
    ///
    /// There was no verb for this at all, so `events_facet.record_event` wrote the table
    /// itself — from 31 call sites across 6 Python modules, i.e. from every session on
    /// the machine, concurrently with the daemon. The single largest remaining
    /// multi-writer violation, and invisible because the writes are small and the helper
    /// swallows every error by design.
    Event {
        kind: String,
        summary: String,
        #[serde(default)]
        session: Option<String>,
        #[serde(default)]
        reference: Option<String>,
        #[serde(default)]
        data: Option<String>,
    },
    /// The operator channel, agent side.
    ///
    /// The DB layer for these already existed in `paos-operator` and was used only by the
    /// daemon, while `operator_facet.py` carried a SECOND implementation for sessions —
    /// the same duplication `accounts` had. These verbs let the Python go.
    OperatorAsk { session: String, question: String, options: Option<String> },
    OperatorAnswer { id: i64, text: String },
    OperatorResolve { id: i64 },
    OperatorPark { session: String, note: String },
    OperatorResolvePark { id: i64 },
    OperatorSay { session: String, text: String },
    OperatorSend { text: String },
    OperatorSetMode { mode: String, by: String },
    /// Delete journal entries older than `days`.
    ///
    /// A DELETE, unlike an append, so it does NOT get the direct fallback `Event` has —
    /// it must not race the daemon.
    EventPrune { days: i64 },
    /// Save a generated standup brief, replacing that side's previous draft.
    StandupBrief { side: String, covers_from: String, covers_to: String, body: String },
    /// Freeze a side's draft and advance its watermark.
    StandupReported { side: String },
    /// Read every setting, or write one.
    ///
    /// `paos_config` is shared by the daemon (which re-reads the dream knobs every tick),
    /// the librarian and the dashboard — and the Python facet wrote it DIRECTLY, from
    /// however many sessions happened to run `paos config set` at once. That is precisely
    /// the multi-writer arrangement paosd exists to remove; it was simply never noticed
    /// because the table is small and the writes are rare.
    ConfigGet,
    ConfigSet { key: String, value: String },
    /// Where does each setting actually come from — `config`, `env`, or `unset`?
    ///
    /// The settings page reads the CONFIG TABLE, but the daemon falls back to the `.env`
    /// beside the store. On 2026-08-03 that gap made the page report a missing Telegram
    /// token and an unconfigured chat id while the bridge was demonstrably working, which
    /// is the exact failure the page's own code comments warn about: a UI that looks like
    /// it worked. Values never cross this boundary, only their origin.
    ConfigSources,
    /// Is this secret configured? The daemon answers with a STATE, never the value —
    /// which is what lets the dashboard render a secret row without the web layer having
    /// any code path that can read a token.
    SecretStatus { key: String },
    /// Queue a proposal for the human-gated review queue.
    ///
    /// A write, so it belongs to the daemon like every other. `memory_proposals` has no
    /// embedding, so the usual single-writer argument does not apply — what decides it is
    /// that `approve` ALREADY cannot finish in a sandbox, because storing the replacement
    /// needs the daemon. Routing the row write the same way makes a sandboxed approve
    /// consistently DEFERRED rather than half-applied: previously the status flipped to
    /// `approved` in SQLite while the fact it was supposed to store went nowhere.
    ///
    /// Deduplication and advisory screening happen daemon-side, because dedup needs to
    /// read the table and doing it in the caller would race.
    ProposalAdd {
        kind: String,
        dataset: String,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        scope: Option<String>,
        /// Comma-joined ids for tidy / split / supersede.
        #[serde(default)]
        target_data_id: Option<String>,
        #[serde(default)]
        rationale: Option<String>,
        #[serde(default)]
        source: Option<String>,
    },
    /// Resolve a proposal — `approved` or `rejected`.
    ///
    /// Guarded on `status='pending'` daemon-side, so a double-approve is a no-op rather
    /// than a second application.
    ProposalSetStatus { id: i64, status: String },
    /// Memory hygiene report.
    MemoryHealth,
    /// Whole-system check: is paos still doing what it claims?
    Doctor,

    /// The supervisor sweeps, which RETURN what they did.
    ///
    /// These exist as daemon verbs rather than spooled writes for one reason: an operator
    /// runs them precisely when they suspect something is wrong, and "queued" cannot
    /// distinguish swept 0 from swept 400 from never ran. A capability that stops working
    /// without erroring is this system's characteristic failure, and answering with
    /// "queued" would install one deliberately.
    BusReap,
    BusPrune { older_than_min: i64 },
    BusPruneRooms,

    // ---- the shared work queue -------------------------------------------------
    //
    // Writes only. Reads open the database directly from the CLI, which is what keeps
    // `paos task list|ready|show` working inside an agent sandbox — where this socket is
    // blocked and every one of the verbs below has to spool instead.
    TaskCreate {
        title: String,
        #[serde(default)]
        body: Option<String>,
        scope: String,
        #[serde(default)]
        org: Option<String>,
        #[serde(default)]
        repo: Option<String>,
        #[serde(default)]
        parent_id: Option<String>,
        #[serde(default = "default_priority")]
        priority: i64,
        origin: String,
        created_by: String,
        #[serde(default)]
        room: Option<String>,
        #[serde(default)]
        start_ready: bool,
    },
    /// The one verb whose spooled form is not good enough on its own — a fire-and-forget
    /// claim tells both racers "spooled" and leaves the loser believing it won. The CLI
    /// follows this with a read-back poll; see `paos-cli/src/task.rs`.
    TaskClaim { id: String, session: String },
    TaskRelease { id: String, session: String },
    TaskState { id: String, to: String, actor: String },
    TaskNote { id: String, author: String, text: String },
    TaskGrant { id: String },
    TaskDep { id: String, depends_on: String, #[serde(default)] remove: bool },
}

fn default_priority() -> i64 { 2 }

fn default_k() -> usize { 8 }

fn default_target() -> String {
    "@all".to_string()
}

/// A reply from the daemon.
///
/// `Ok` carries pre-rendered lines because the CLI is deliberately dumb: rendering
/// lives on the side that owns the data, so a protocol change never requires shipping a
/// new CLI. `Err` carries an exit code so the caller's `$?` is meaningful — the Python
/// implementation discarded handler return values and every command exited 0.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum Response {
    Ok { lines: Vec<String> },
    Err { message: String, exit_code: i32 },
}

impl Response {
    pub fn ok<S: Into<String>>(line: S) -> Self {
        Response::Ok { lines: vec![line.into()] }
    }
    pub fn err<S: Into<String>>(message: S, exit_code: i32) -> Self {
        Response::Err { message: message.into(), exit_code }
    }
}

/// Write one length-prefixed JSON frame.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, value: &T) -> io::Result<()> {
    let body = serde_json::to_vec(value).map_err(io::Error::other)?;
    let len: u32 = body
        .len()
        .try_into()
        .map_err(|_| io::Error::other("frame exceeds u32"))?;
    if len > MAX_FRAME {
        return Err(io::Error::other("frame too large"));
    }
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// Read one length-prefixed JSON frame.
pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME {
        return Err(io::Error::other("frame too large"));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_request() {
        let req = Request::Whoami { session_id: Some("abc".into()) };
        let mut buf = Vec::new();
        write_frame(&mut buf, &req).unwrap();
        let got: Request = read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(got, req);
    }

    #[test]
    fn round_trips_a_response() {
        let res = Response::ok("swift-otter");
        let mut buf = Vec::new();
        write_frame(&mut buf, &res).unwrap();
        let got: Response = read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(got, res);
    }

    #[test]
    fn two_frames_on_one_stream_stay_separate() {
        // The length prefix exists so a reader never has to guess where a frame ends.
        let mut buf = Vec::new();
        write_frame(&mut buf, &Request::Ping).unwrap();
        write_frame(&mut buf, &Request::Version).unwrap();
        let mut cur = buf.as_slice();
        assert_eq!(read_frame::<_, Request>(&mut cur).unwrap(), Request::Ping);
        assert_eq!(read_frame::<_, Request>(&mut cur).unwrap(), Request::Version);
    }

    #[test]
    fn oversized_length_is_refused_without_allocating() {
        // A wedged or hostile stream must not make us allocate 4 GiB.
        let mut buf = Vec::new();
        buf.extend_from_slice(&u32::MAX.to_be_bytes());
        let err = read_frame::<_, Request>(&mut buf.as_slice()).unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn truncated_frame_is_an_error_not_a_partial_parse() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Request::Ping).unwrap();
        buf.truncate(buf.len() - 1);
        assert!(read_frame::<_, Request>(&mut buf.as_slice()).is_err());
    }

    #[test]
    fn error_response_carries_a_usable_exit_code() {
        // The Python CLI discarded handler return values, so every command exited 0.
        match Response::err("no daemon", 3) {
            Response::Err { exit_code, .. } => assert_eq!(exit_code, 3),
            _ => panic!("expected Err"),
        }
    }
}
