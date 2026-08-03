//! The work queue, rendered for a phone.
//!
//! Deliberately NOT a board — five columns do not fit on a phone. But it is a LIST, not
//! a summary: it leads with the items that need a decision and puts a button on each,
//! then lists the open queue by state. The first version reduced everything past "needs
//! you" to per-column counts, and the operator's answer was "I want to tap tasks and be
//! able to see them. Now I only see counts." A count says whether the queue is growing;
//! it does not say what is in it, which is the question a tasks button is tapped to ask.
//!
//! The board at 127.0.0.1:8788 stays the place to actually work the queue.

use paos_tasks::model::State;
use paos_tasks::{query, store};
use rusqlite::Connection;

/// How many actionable items get buttons. Telegram will render more, but a phone screen
/// full of buttons is a wall, not a decision — the rest are named in the overflow line.
const MAX_ACTIONABLE: usize = 5;

/// How many tasks to list per state. Enough to see the queue, few enough that the message
/// stays scannable on a phone; the overflow is counted rather than hidden.
const MAX_PER_STATE: usize = 8;

/// What needs the operator, then the actual tasks.
///
/// This LISTS the queue rather than summarising it. It summarised at first — one counts
/// line per column — and the operator's response was "I want to tap tasks and be able to
/// see them. Now I only see counts." A count answers "is it growing"; it does not answer
/// "what is in there", which is the question someone taps a tasks button to ask.
///
/// Only `done` and `dropped` stay as counts. They are history, and history is the one
/// thing a phone genuinely does not want scrolled at it.
pub fn digest(conn: &Connection) -> String {
    let all = match query::all(conn) {
        Ok(t) => t,
        Err(e) => return format!("could not read tasks: {e}"),
    };
    if all.is_empty() {
        return "📋 no tasks yet\n\ncreate one: paos task create \"…\"".into();
    }
    let blocked = query::blocked_ids(conn).unwrap_or_default();

    let actionable = actionable(&all);
    let mut out = String::from("📋 tasks\n");

    if !actionable.is_empty() {
        out.push_str(&format!("\nNEEDS YOU ({})\n", actionable.len()));
        for t in actionable.iter().take(MAX_ACTIONABLE) {
            out.push_str(&format!("• {}  {}\n   {}\n", t.id, t.title, why(t)));
        }
        if actionable.len() > MAX_ACTIONABLE {
            out.push_str(&format!("  …and {} more\n", actionable.len() - MAX_ACTIONABLE));
        }
    }

    // The open queue, listed. Newest state first so the eye lands on work in flight.
    for state in [State::InProgress, State::Review, State::Ready, State::Proposed] {
        let mut rows: Vec<&paos_tasks::model::Task> =
            all.iter().filter(|t| t.state == state).collect();
        if rows.is_empty() {
            continue;
        }
        rows.sort_by_key(|t| (t.priority, t.created_ts.clone()));
        out.push_str(&format!("\n{} ({})\n",
                              state.as_str().replace('_', " ").to_uppercase(), rows.len()));
        for t in rows.iter().take(MAX_PER_STATE) {
            let mut tail = Vec::new();
            if t.priority != 2 {
                tail.push(format!("p{}", t.priority));
            }
            match (&t.claimed_by, &t.last_owner) {
                (Some(w), _) => tail.push(format!("@{w}")),
                (None, Some(p)) if t.orphaned => tail.push(format!("⤺ was @{p}")),
                (None, Some(p)) => tail.push(format!("released by {p}")),
                _ => {}
            }
            if blocked.contains(&t.id) {
                tail.push("⛔ blocked".into());
            }
            if let Some(r) = &t.repo {
                tail.push(r.clone());
            }
            let suffix = if tail.is_empty() { String::new() }
                         else { format!("  · {}", tail.join(" · ")) };
            out.push_str(&format!("• {}  {}{}\n", t.id, t.title, suffix));
        }
        if rows.len() > MAX_PER_STATE {
            out.push_str(&format!("  …and {} more\n", rows.len() - MAX_PER_STATE));
        }
    }

    let finished = all.iter().filter(|t| t.state.is_terminal()).count();
    if finished > 0 {
        out.push_str(&format!("\n{finished} done or dropped\n"));
    }
    out.push_str("\nfull board: http://127.0.0.1:8788 → tasks");
    out
}

/// Work only the operator can move.
///
/// Same predicate as the dashboard badge (`needs_operator`), and for the same reason:
/// `proposed` is excluded because the operator wrote those themselves, so listing them
/// as things demanding attention is noise.
pub fn actionable(all: &[paos_tasks::model::Task]) -> Vec<&paos_tasks::model::Task> {
    let mut v: Vec<&paos_tasks::model::Task> = all
        .iter()
        .filter(|t| {
            let awaiting_approval = t.state == State::Review
                && t.origin == paos_tasks::model::Origin::Operator
                && !t.close_grant;
            let dropped_on_the_floor =
                t.orphaned && t.is_unowned() && !t.state.is_terminal();
            awaiting_approval || dropped_on_the_floor
        })
        .collect();
    v.sort_by_key(|t| (t.priority, t.created_ts.clone()));
    v
}

fn why(t: &paos_tasks::model::Task) -> String {
    if t.orphaned && t.is_unowned() {
        format!("⤺ {} ended holding it — still {}",
                t.last_owner.as_deref().unwrap_or("a session"), t.state.as_str())
    } else {
        format!("awaiting your approval · from {}", t.created_by)
    }
}

/// One row of buttons per actionable task.
///
/// `callback_data` is `task:<verb>:<id>` — 18 bytes for a real id, comfortably inside
/// Telegram's 64-byte cap.
pub fn markup(conn: &Connection) -> Option<String> {
    let all = query::all(conn).ok()?;
    let items = actionable(&all);
    if items.is_empty() {
        return None;
    }
    let mut rows: Vec<String> = Vec::new();
    for t in items.iter().take(MAX_ACTIONABLE) {
        let short: String = t.title.chars().take(18).collect();
        let row = if t.orphaned && t.is_unowned() {
            // A dropped task needs a decision about whether it still matters, not an
            // approval — nobody has done the work yet.
            format!(
                "[{{\"text\":\"🗑 drop {}\",\"callback_data\":\"task:drop:{}\"}},\
                  {{\"text\":\"👁 {}\",\"callback_data\":\"task:show:{}\"}}]",
                esc(&t.id), t.id, esc(&short), t.id)
        } else {
            format!(
                "[{{\"text\":\"✓ approve {}\",\"callback_data\":\"task:done:{}\"}},\
                  {{\"text\":\"🤝 let a session finish it\",\"callback_data\":\"task:grant:{}\"}}]",
                esc(&t.id), t.id, t.id)
        };
        rows.push(row);
    }
    Some(format!("{{\"inline_keyboard\":[{}]}}", rows.join(",")))
}


// ---- drill-down: repo → state → tasks -------------------------------------
//
// "I want to select the repo, then the state. On every step i want to see the count of
// tasks inside." Three screens, each one a list of buttons carrying its own count, so the
// number is never something you have to tap to discover.

/// One rendered screen: what to show, what buttons to show under it, and what to flash
/// on the tap itself.
pub struct Screen {
    pub body: String,
    pub markup: Option<String>,
    pub toast: String,
}

/// A short, stable handle for a repo inside `callback_data`.
///
/// Telegram caps callback_data at 64 bytes and repo names here run to 36 characters
/// (`examplecorp_motion_client_dashboard_ops`), which does not leave room for a prefix and a
/// state. An INDEX into the repo list would fit but goes stale the moment a task is
/// created between render and tap — and a stale index silently opens the wrong repo,
/// which is worse than an error. A hash of the name is stable across renders.
fn repo_key(repo: Option<&str>) -> String {
    let name = repo.unwrap_or("");
    if name.is_empty() {
        return "-".into();
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("{:06x}", h & 0xff_ffff)
}

/// Every repo that has open work, with its count. `None` is the no-repo bucket, which
/// holds the global and org-scoped tasks.
fn repo_buckets(all: &[paos_tasks::model::Task]) -> Vec<(Option<String>, usize)> {
    let mut seen: Vec<(Option<String>, usize)> = Vec::new();
    for t in all.iter().filter(|t| !t.state.is_terminal()) {
        let key = t.repo.clone();
        match seen.iter_mut().find(|(r, _)| *r == key) {
            Some((_, n)) => *n += 1,
            None => seen.push((key, 1)),
        }
    }
    // Biggest first: the repo with the most open work is the one most likely wanted.
    seen.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    seen
}

fn resolve_repo(all: &[paos_tasks::model::Task], key: &str) -> Option<Option<String>> {
    if key == "*" {
        return None;
    }
    repo_buckets(all)
        .into_iter()
        .map(|(r, _)| r)
        .find(|r| repo_key(r.as_deref()) == key)
}

fn label(repo: &Option<String>) -> String {
    repo.clone().unwrap_or_else(|| "no repo".into())
}

fn btn(text: &str, data: &str) -> String {
    format!("{{\"text\":\"{}\",\"callback_data\":\"{}\"}}", esc(text), esc(data))
}

fn rows_to_markup(rows: Vec<Vec<String>>) -> Option<String> {
    let rows: Vec<String> = rows
        .into_iter()
        .filter(|r| !r.is_empty())
        .map(|r| format!("[{}]", r.join(",")))
        .collect();
    if rows.is_empty() {
        return None;
    }
    Some(format!("{{\"inline_keyboard\":[{}]}}", rows.join(",")))
}

/// Screen 1 — pick a repo. Counts on every button.
pub fn screen_repos(conn: &Connection) -> Screen {
    let all = query::all(conn).unwrap_or_default();
    let open = all.iter().filter(|t| !t.state.is_terminal()).count();
    if all.is_empty() {
        return Screen {
            body: "📋 no tasks yet\n\ncreate one: paos task create \"…\"".into(),
            markup: None,
            toast: "no tasks".into(),
        };
    }
    let needs = actionable(&all).len();
    let mut body = format!("📋 tasks — {open} open");
    if needs > 0 {
        body.push_str(&format!("\n⚠ {needs} waiting on you"));
    }
    body.push_str("\n\npick a repo");

    let buckets = repo_buckets(&all);
    let mut rows: Vec<Vec<String>> = Vec::new();
    if needs > 0 {
        rows.push(vec![btn(&format!("⚠ needs you {needs}"), "task:needs")]);
    }
    for pair in buckets.chunks(2) {
        rows.push(
            pair.iter()
                .map(|(r, n)| {
                    btn(&format!("{} {n}", label(r)),
                        &format!("task:r:{}", repo_key(r.as_deref())))
                })
                .collect(),
        );
    }
    rows.push(vec![btn(&format!("everything {open}"), "task:r:*")]);
    Screen { body, markup: rows_to_markup(rows), toast: "tasks".into() }
}

/// Screen 2 — pick a state within a repo. Counts again, and states with none are omitted
/// rather than shown as zero: an empty column is not a destination.
fn screen_states(conn: &Connection, key: &str) -> Screen {
    let all = query::all(conn).unwrap_or_default();
    let repo = resolve_repo(&all, key);
    let name = match (&repo, key) {
        (Some(r), _) => label(r),
        (None, "*") => "everything".into(),
        _ => return screen_repos(conn),
    };
    let in_scope: Vec<&paos_tasks::model::Task> = all
        .iter()
        .filter(|t| repo.as_ref().map(|r| &t.repo == r).unwrap_or(true))
        .collect();
    let open = in_scope.iter().filter(|t| !t.state.is_terminal()).count();

    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    for st in [State::InProgress, State::Review, State::Ready, State::Proposed,
               State::Done, State::Dropped] {
        let n = in_scope.iter().filter(|t| t.state == st).count();
        if n == 0 {
            continue;
        }
        row.push(btn(&format!("{} {n}", st.as_str().replace('_', " ")),
                     &format!("task:s:{key}:{}", st.as_str())));
        if row.len() == 2 {
            rows.push(std::mem::take(&mut row));
        }
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows.push(vec![btn("‹ repos", "task:repos")]);
    Screen {
        body: format!("📋 {name} — {open} open\n\npick a state"),
        markup: rows_to_markup(rows),
        toast: name,
    }
}

/// Screen 3 — the tasks themselves.
fn screen_list(conn: &Connection, key: &str, state: &str) -> Screen {
    let all = query::all(conn).unwrap_or_default();
    let Some(st) = State::parse(state) else { return screen_repos(conn) };
    let repo = resolve_repo(&all, key);
    let name = match (&repo, key) {
        (Some(r), _) => label(r),
        (None, "*") => "everything".into(),
        _ => return screen_repos(conn),
    };
    let blocked = query::blocked_ids(conn).unwrap_or_default();
    let mut rows: Vec<&paos_tasks::model::Task> = all
        .iter()
        .filter(|t| t.state == st)
        .filter(|t| repo.as_ref().map(|r| &t.repo == r).unwrap_or(true))
        .collect();
    rows.sort_by_key(|t| (t.priority, t.created_ts.clone()));

    let mut body = format!("📋 {name} · {} ({})\n", st.as_str().replace('_', " "), rows.len());
    for t in rows.iter().take(MAX_PER_STATE) {
        body.push_str(&format!("\n• {}  {}{}\n", t.id, t.title, tail_for(t, &blocked)));
    }
    if rows.len() > MAX_PER_STATE {
        body.push_str(&format!("\n…and {} more\n", rows.len() - MAX_PER_STATE));
    }

    // A button per task, capped: a screen of buttons is a wall, not a choice.
    let mut kb: Vec<Vec<String>> = Vec::new();
    for t in rows.iter().take(MAX_ACTIONABLE) {
        let short: String = t.title.chars().take(24).collect();
        kb.push(vec![btn(&format!("👁 {short}"), &format!("task:show:{}", t.id))]);
    }
    kb.push(vec![btn("‹ states", &format!("task:r:{key}")),
                 btn("‹ repos", "task:repos")]);
    Screen { body, markup: rows_to_markup(kb), toast: st.as_str().into() }
}

fn tail_for(t: &paos_tasks::model::Task,
            blocked: &std::collections::HashSet<String>) -> String {
    let mut tail = Vec::new();
    if t.priority != 2 {
        tail.push(format!("p{}", t.priority));
    }
    match (&t.claimed_by, &t.last_owner) {
        (Some(w), _) => tail.push(format!("@{w}")),
        (None, Some(p)) if t.orphaned => tail.push(format!("⤺ was @{p}")),
        _ => {}
    }
    if blocked.contains(&t.id) {
        tail.push("⛔ blocked".into());
    }
    if tail.is_empty() { String::new() } else { format!("  · {}", tail.join(" · ")) }
}

/// Everything under the `task:` callback prefix — navigation and actions both.
///
/// One entry point so `bridge.rs` stays a delegation rather than growing a second copy of
/// the task rules.
pub fn callback(conn: &Connection, rest: &str, now: &str) -> Screen {
    match rest.split_once(':') {
        None if rest == "repos" => screen_repos(conn),
        None if rest == "needs" => {
            let body = digest(conn);
            let mut kb = match markup(conn) {
                Some(m) => vec![m],
                None => vec![],
            };
            kb.clear();
            Screen {
                body,
                markup: markup(conn).map(|m| {
                    // Splice a back button onto the action rows.
                    let inner = m.trim_start_matches("{\"inline_keyboard\":[")
                                 .trim_end_matches("]}");
                    format!("{{\"inline_keyboard\":[{inner},[{}]]}}",
                            btn("‹ repos", "task:repos"))
                }).or_else(|| rows_to_markup(vec![vec![btn("‹ repos", "task:repos")]])),
                toast: "needs you".into(),
            }
        }
        None => screen_repos(conn),
        Some(("r", key)) => screen_states(conn, key),
        Some(("s", rest)) => match rest.split_once(':') {
            Some((key, state)) => screen_list(conn, key, state),
            None => screen_repos(conn),
        },
        // Actions. After one, drop back to the repo picker with the result on top, so the
        // counts the operator is looking at are the ones after their tap.
        Some((verb, id)) => {
            let out = apply(conn, verb, id, now);
            if verb == "show" {
                return Screen {
                    body: out,
                    markup: rows_to_markup(vec![vec![btn("‹ repos", "task:repos")]]),
                    toast: "briefing".into(),
                };
            }
            let mut s = screen_repos(conn);
            s.body = format!("{out}\n\n{}", s.body);
            s.toast = out;
            s
        }
    }
}

/// Apply a tapped button. Returns the line to show the operator.
///
/// Every action here is the OPERATOR acting — this is their Telegram — so `Actor::Operator`
/// is correct and `may_close` will let it through. It still goes through `set_state` rather
/// than a direct UPDATE, so the transition rules and the note trail are the same ones the
/// CLI and the board get.
pub fn apply(conn: &Connection, verb: &str, id: &str, now: &str) -> String {
    match verb {
        "show" => match store::get(conn, id) {
            Ok(Some(t)) => {
                let mut s = format!("{}  [{}]  p{}\n{}\n", t.id, t.state.as_str(),
                                    t.priority, t.title);
                if let Some(b) = &t.body {
                    s.push_str(&format!("\n{b}\n"));
                }
                match query::notes(conn, id) {
                    Ok(ns) if !ns.is_empty() => {
                        s.push_str("\nlog:\n");
                        // Newest last, but only the tail — a phone does not want the
                        // whole history, it wants where things got to.
                        for n in ns.iter().rev().take(6).rev() {
                            s.push_str(&format!("  {} — {}\n", n.author, n.text));
                        }
                    }
                    _ => {}
                }
                s
            }
            Ok(None) => format!("no such task: {id}"),
            Err(e) => format!("could not read {id}: {e}"),
        },
        "done" | "drop" => {
            let to = if verb == "done" { State::Done } else { State::Dropped };
            match store::set_state(conn, id, to, &store::Actor::Operator, now) {
                Ok(()) => format!("{id} → {}", to.as_str()),
                Err(e) => format!("could not: {e}"),
            }
        }
        "grant" => match store::grant_close(conn, id, now) {
            Ok(()) => format!("{id}: a session may now close this without you"),
            Err(e) => format!("could not: {e}"),
        },
        _ => format!("unknown action: {verb}"),
    }
}

/// Escape for a JSON string literal in the inline-keyboard payload.
///
/// Task ids are safe, but titles are arbitrary text a session wrote, and an unescaped
/// quote would produce malformed JSON that Telegram rejects — the whole keyboard would
/// silently fail to render.
fn esc(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            '\n' | '\r' | '\t' => vec![' '],
            c if (c as u32) < 0x20 => vec![' '],
            c => vec![c],
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use paos_tasks::model::{NewTask, Origin};

    fn db() -> Connection {
        paos_store::open_in_memory().unwrap()
    }
    fn a_task(title: &str) -> NewTask {
        NewTask {
            title: title.into(), body: None, scope: "global".into(), org: None, repo: None,
            parent_id: None, priority: 2, origin: Origin::Session,
            created_by: "swift-otter".into(), room: None, start_ready: false,
        }
    }
    const T0: &str = "2026-08-02T00:00:00Z";
    const T1: &str = "2026-08-02T00:01:00Z";

    #[test]
    fn an_empty_queue_says_so_rather_than_rendering_an_empty_board() {
        assert!(digest(&db()).contains("no tasks yet"));
    }

    #[test]
    fn a_proposed_task_does_not_count_as_needing_the_operator() {
        // They wrote it themselves. Listing their own backlog as a demand is the noise
        // that stops a digest being read.
        let c = db();
        let mut op = a_task("just an idea");
        op.origin = Origin::Operator;
        store::create(&c, &op, T0).unwrap();
        let d = digest(&c);
        assert!(!d.contains("NEEDS YOU"), "their own backlog must not demand attention: {d}");
        assert!(d.contains("PROPOSED (1)"), "but it is still listed: {d}");
        assert!(markup(&c).is_none());
    }

    #[test]
    fn a_review_item_awaiting_approval_gets_an_approve_button() {
        let c = db();
        let mut op = a_task("finish me");
        op.origin = Origin::Operator;
        let id = store::create(&c, &op, T0).unwrap();
        store::set_state(&c, &id, State::Review, &store::Actor::Session("swift-otter"), T1)
            .unwrap();

        let d = digest(&c);
        assert!(d.contains("NEEDS YOU (1)"), "{d}");
        assert!(d.contains("awaiting your approval"), "{d}");
        let m = markup(&c).expect("a button");
        assert!(m.contains(&format!("task:done:{id}")), "{m}");
        assert!(m.contains(&format!("task:grant:{id}")), "{m}");
    }

    #[test]
    fn a_granted_review_item_no_longer_needs_the_operator() {
        let c = db();
        let mut op = a_task("you finish it");
        op.origin = Origin::Operator;
        let id = store::create(&c, &op, T0).unwrap();
        store::grant_close(&c, &id, T0).unwrap();
        store::set_state(&c, &id, State::Review, &store::Actor::Session("swift-otter"), T1)
            .unwrap();
        let d = digest(&c);
        assert!(!d.contains("NEEDS YOU"),
                "granting close-authority is exactly what takes it off their plate: {d}");
        assert!(d.contains("REVIEW (1)"), "it is still visible in the queue: {d}");
    }

    #[test]
    fn an_orphan_offers_drop_and_briefing_not_approve() {
        // Nobody has finished this work, so "approve" would be meaningless — the decision
        // is whether it still matters.
        let c = db();
        let id = store::create(&c, &a_task("half done"), T0).unwrap();
        c.execute("UPDATE tasks SET state='in_progress', orphaned=1, last_owner='swift-otter' \
                   WHERE id=?1", [&id]).unwrap();
        let d = digest(&c);
        assert!(d.contains("swift-otter ended holding it"), "{d}");
        let m = markup(&c).expect("buttons");
        assert!(m.contains(&format!("task:drop:{id}")));
        assert!(m.contains(&format!("task:show:{id}")));
        assert!(!m.contains("task:done:"));
    }

    /// The regression that started this: tapping tasks showed counts and no tasks.
    #[test]
    fn the_open_queue_is_listed_by_title_not_summarised() {
        let c = db();
        let a = store::create(&c, &a_task("write the changelog"), T0).unwrap();
        store::create(&c, &a_task("rebase the branch"), T1).unwrap();
        let d = digest(&c);
        assert!(d.contains("READY (2)"), "{d}");
        assert!(d.contains("write the changelog"), "titles must be visible: {d}");
        assert!(d.contains("rebase the branch"), "{d}");
        assert!(d.contains(&a), "ids must be visible so they can be acted on: {d}");
    }

    #[test]
    fn a_listed_task_shows_who_holds_it_and_whether_it_is_blocked() {
        let c = db();
        let dep = store::create(&c, &a_task("first"), T0).unwrap();
        let held = store::create(&c, &a_task("second"), T1).unwrap();
        store::dep_add(&c, &held, &dep, T1).unwrap();
        c.execute("UPDATE tasks SET state='in_progress', claimed_by='swift-otter' WHERE id=?1",
                  [&held]).unwrap();
        let d = digest(&c);
        assert!(d.contains("@swift-otter"), "{d}");
        assert!(d.contains("⛔ blocked"), "{d}");
    }

    #[test]
    fn finished_work_stays_a_count_because_history_is_not_worth_scrolling() {
        let c = db();
        let id = store::create(&c, &a_task("finished"), T0).unwrap();
        store::set_state(&c, &id, State::Done, &store::Actor::Operator, T1).unwrap();
        let d = digest(&c);
        assert!(d.contains("1 done or dropped"), "{d}");
        assert!(!d.contains("DONE ("), "done must not get a listed section: {d}");
    }

    #[test]
    fn a_long_queue_is_capped_and_says_how_much_it_hid() {
        let c = db();
        for i in 0..12 {
            store::create(&c, &a_task(&format!("task {i}")), &format!("2026-08-02T00:{i:02}:00Z"))
                .unwrap();
        }
        let d = digest(&c);
        assert!(d.contains("READY (12)"), "the true total must be stated: {d}");
        assert!(d.contains("…and 4 more"), "silent truncation reads as completeness: {d}");
    }

    #[test]
    fn tapping_approve_closes_the_task_through_the_normal_rules() {
        let c = db();
        let mut op = a_task("x");
        op.origin = Origin::Operator;
        let id = store::create(&c, &op, T0).unwrap();
        let out = apply(&c, "done", &id, T1);
        assert!(out.contains("done"), "{out}");
        assert_eq!(store::get(&c, &id).unwrap().unwrap().state, State::Done);
        // …and left the note trail, so the board shows how it was closed.
        let notes = query::notes(&c, &id).unwrap();
        assert!(notes.iter().any(|n| n.author == "operator"), "operator's action is recorded");
    }

    #[test]
    fn tapping_a_refused_action_reports_the_reason_rather_than_claiming_success() {
        let c = db();
        let id = store::create(&c, &a_task("x"), T0).unwrap();
        store::set_state(&c, &id, State::Dropped, &store::Actor::Operator, T1).unwrap();
        let out = apply(&c, "done", &id, T1);
        assert!(out.starts_with("could not:"), "{out}");
    }

    #[test]
    fn a_title_with_a_quote_cannot_break_the_keyboard_json() {
        let c = db();
        let id = store::create(&c, &a_task("a \"quoted\" one"), T0).unwrap();
        c.execute("UPDATE tasks SET state='in_progress', orphaned=1, last_owner='x' \
                   WHERE id=?1", [&id]).unwrap();
        let m = markup(&c).expect("buttons");
        assert!(m.contains("\\\""), "the quote must be escaped: {m}");
        assert!(!m.contains("\"a \"quoted\""), "raw quote leaked into the JSON");
    }

    #[test]
    fn the_briefing_shows_the_tail_of_the_log() {
        let c = db();
        let id = store::create(&c, &a_task("x"), T0).unwrap();
        for i in 0..9 {
            store::note(&c, &id, "swift-otter", "note", &format!("step {i}"), T1).unwrap();
        }
        let out = apply(&c, "show", &id, T1);
        assert!(out.contains("step 8"), "the newest must be there: {out}");
        assert!(!out.contains("step 0"), "the whole history must not be: {out}");
    }
    // ---- drill-down ------------------------------------------------------

    fn with_repo(c: &Connection, title: &str, repo: &str, ts: &str) -> String {
        let mut n = a_task(title);
        n.scope = "project".into();
        n.repo = Some(repo.into());
        store::create(c, &n, ts).unwrap()
    }

    #[test]
    fn the_repo_picker_counts_open_work_per_repo() {
        let c = db();
        with_repo(&c, "one", "dotfiles", T0);
        with_repo(&c, "two", "dotfiles", T1);
        with_repo(&c, "three", "agentic-brain", T1);
        store::create(&c, &a_task("global thing"), T1).unwrap();

        let s = screen_repos(&c);
        assert!(s.body.contains("4 open"), "{}", s.body);
        let m = s.markup.expect("buttons");
        assert!(m.contains("dotfiles 2"), "{m}");
        assert!(m.contains("agentic-brain 1"), "{m}");
        assert!(m.contains("no repo 1"), "global/org tasks need a bucket: {m}");
        assert!(m.contains("everything 4"), "{m}");
    }

    #[test]
    fn a_repo_key_survives_a_task_being_added_between_render_and_tap() {
        // The reason this hashes the name instead of indexing the list: an index would
        // shift when another session creates a task, and the next tap would silently open
        // a different repo.
        let c = db();
        with_repo(&c, "one", "dotfiles", T0);
        let key = repo_key(Some("dotfiles"));
        with_repo(&c, "later", "aaa-sorts-first", T1);
        let all = query::all(&c).unwrap();
        assert_eq!(resolve_repo(&all, &key), Some(Some("dotfiles".into())));
    }

    #[test]
    fn finished_work_is_excluded_from_the_repo_counts() {
        let c = db();
        let id = with_repo(&c, "done one", "dotfiles", T0);
        with_repo(&c, "open one", "dotfiles", T1);
        store::set_state(&c, &id, State::Done, &store::Actor::Operator, T1).unwrap();
        let m = screen_repos(&c).markup.unwrap();
        assert!(m.contains("dotfiles 1"), "a closed task is not open work: {m}");
    }

    #[test]
    fn the_state_screen_counts_within_the_chosen_repo_only() {
        let c = db();
        with_repo(&c, "here ready", "dotfiles", T0);
        let held = with_repo(&c, "here running", "dotfiles", T1);
        c.execute("UPDATE tasks SET state='in_progress' WHERE id=?1", [&held]).unwrap();
        with_repo(&c, "elsewhere", "other", T1);

        let key = repo_key(Some("dotfiles"));
        let s = callback(&c, &format!("r:{key}"), T1);
        assert!(s.body.contains("dotfiles — 2 open"), "{}", s.body);
        let m = s.markup.unwrap();
        assert!(m.contains("ready 1") && m.contains("in progress 1"), "{m}");
        assert!(!m.contains("review"), "a state with nothing in it is not a destination: {m}");
    }

    #[test]
    fn the_task_list_shows_only_that_repo_and_that_state() {
        let c = db();
        with_repo(&c, "wanted", "dotfiles", T0);
        with_repo(&c, "other repo", "elsewhere", T1);
        let running = with_repo(&c, "wrong state", "dotfiles", T1);
        c.execute("UPDATE tasks SET state='in_progress' WHERE id=?1", [&running]).unwrap();

        let key = repo_key(Some("dotfiles"));
        let s = callback(&c, &format!("s:{key}:ready"), T1);
        assert!(s.body.contains("wanted"), "{}", s.body);
        assert!(!s.body.contains("other repo"), "{}", s.body);
        assert!(!s.body.contains("wrong state"), "{}", s.body);
    }

    #[test]
    fn every_screen_offers_a_way_back() {
        let c = db();
        with_repo(&c, "one", "dotfiles", T0);
        let key = repo_key(Some("dotfiles"));
        for data in [format!("r:{key}"), format!("s:{key}:ready")] {
            let m = callback(&c, &data, T1).markup.expect("buttons");
            assert!(m.contains("task:repos"), "no way back from {data}: {m}");
        }
    }

    #[test]
    fn everything_spans_all_repos() {
        let c = db();
        with_repo(&c, "one", "dotfiles", T0);
        with_repo(&c, "two", "other", T1);
        let s = callback(&c, "s:*:ready", T1);
        assert!(s.body.contains("one") && s.body.contains("two"), "{}", s.body);
    }

    #[test]
    fn an_unknown_repo_key_falls_back_to_the_picker_rather_than_a_dead_end() {
        let c = db();
        with_repo(&c, "one", "dotfiles", T0);
        let s = callback(&c, "r:zzzzzz", T1);
        assert!(s.body.contains("pick a repo"), "{}", s.body);
    }

    #[test]
    fn acting_from_a_button_returns_to_the_picker_with_updated_counts() {
        let c = db();
        let mut op = a_task("approve me");
        op.origin = paos_tasks::model::Origin::Operator;
        let id = store::create(&c, &op, T0).unwrap();
        store::set_state(&c, &id, State::Review, &store::Actor::Session("x"), T1).unwrap();
        let s = callback(&c, &format!("done:{id}"), T1);
        assert!(s.body.starts_with(&format!("{id} → done")), "{}", s.body);
        assert!(s.body.contains("pick a repo"), "the counts after the tap: {}", s.body);
    }

}
