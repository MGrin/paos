//! Types shared by the store and the queries.

/// Where a task is in its life.
///
/// `blocked` is deliberately absent. It is a query over unmet dependencies, never a
/// stored value: a stored flag needs something to keep it true, and nothing here would.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Proposed,
    Ready,
    InProgress,
    Review,
    Done,
    Dropped,
}

impl State {
    pub fn as_str(&self) -> &'static str {
        match self {
            State::Proposed => "proposed",
            State::Ready => "ready",
            State::InProgress => "in_progress",
            State::Review => "review",
            State::Done => "done",
            State::Dropped => "dropped",
        }
    }

    pub fn parse(s: &str) -> Option<State> {
        Some(match s {
            "proposed" => State::Proposed,
            "ready" => State::Ready,
            "in_progress" => State::InProgress,
            "review" => State::Review,
            "done" => State::Done,
            "dropped" => State::Dropped,
            _ => return None,
        })
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, State::Done | State::Dropped)
    }

    /// The board's columns, left to right. `dropped` is not among them — it is terminal
    /// and hidden by default.
    pub const COLUMNS: [State; 5] = [
        State::Proposed,
        State::Ready,
        State::InProgress,
        State::Review,
        State::Done,
    ];
}

/// Who created a task. This is what decides close authority, so it is recorded at
/// creation and never changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Operator,
    Session,
}

impl Origin {
    pub fn as_str(&self) -> &'static str {
        match self {
            Origin::Operator => "operator",
            Origin::Session => "session",
        }
    }

    pub fn parse(s: &str) -> Option<Origin> {
        Some(match s {
            "operator" => Origin::Operator,
            "session" => Origin::Session,
            _ => return None,
        })
    }
}

pub struct NewTask {
    pub title: String,
    pub body: Option<String>,
    pub scope: String,
    pub org: Option<String>,
    pub repo: Option<String>,
    pub parent_id: Option<String>,
    pub priority: i64,
    pub origin: Origin,
    pub created_by: String,
    pub room: Option<String>,
    /// Operator-created tasks land in `proposed` for triage. This skips that, which
    /// matters when the operator is away and wants the work picked up now.
    pub start_ready: bool,
}

#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub body: Option<String>,
    pub state: State,
    pub priority: i64,
    pub scope: String,
    pub org: Option<String>,
    pub repo: Option<String>,
    pub parent_id: Option<String>,
    pub origin: Origin,
    pub created_by: String,
    pub claimed_by: Option<String>,
    pub claimed_ts: Option<String>,
    pub last_owner: Option<String>,
    pub orphaned: bool,
    pub close_grant: bool,
    pub room: Option<String>,
    pub created_ts: String,
    pub updated_ts: String,
    pub closed_ts: Option<String>,
}

impl Task {
    /// Ownership is ONE predicate everywhere: `claimed_by IS NULL`.
    ///
    /// `orphaned` records only *how* a task became unowned — its session died, versus a
    /// voluntary `release` — and is never what a query keys off. Keying on it made a
    /// released task invisible to both `ready` (its state is `in_progress`) and the
    /// orphan view (`orphaned` is 0): work owned by nobody and findable by no query.
    pub fn is_unowned(&self) -> bool {
        self.claimed_by.is_none()
    }

    /// Work a session can pick up: unowned, not finished, and not parked in `review`
    /// awaiting the operator.
    pub fn is_claimable(&self) -> bool {
        self.is_unowned()
            && matches!(self.state, State::Proposed | State::Ready | State::InProgress)
    }

    /// An unowned task that already has progress on it. The most valuable row on the
    /// board: someone did work, and it is sitting there finished-enough to continue.
    pub fn is_rescue(&self) -> bool {
        self.is_unowned() && self.state == State::InProgress
    }
}
