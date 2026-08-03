//! The board's server side.
//!
//! Split out of `lib.rs` rather than appended to it: that file was already carrying
//! every other view, and a kanban board's payload plus five write routes is not a
//! footnote. Same crate, so `impl Web` here is the same type — only the file is smaller.
//!
//! The dashboard always acts as the OPERATOR. That is what makes the board able to
//! approve `review` items, and it is why `may_close` is consulted rather than assumed:
//! the policy lives in `paos-tasks` and neither caller reimplements it.

use crate::http::{Request, Response};
use crate::{field, now_iso, Web};
use paos_tasks::model::{Origin, State, Task};
use paos_tasks::{query, store};

const TASKS_CSS: &str = include_str!("tasks.css");
const TASKS_JS: &str = include_str!("tasks.js");

pub fn asset(path: &str) -> Option<Response> {
    match path {
        "/tasks.css" => Some(Response::css(TASKS_CSS)),
        "/tasks.js" => Some(Response::js(TASKS_JS)),
        _ => None,
    }
}

impl Web {
    /// The whole board in one payload.
    ///
    /// Three queries and a join in Rust, rather than a correlated subquery per card: the
    /// board renders a few hundred rows at once, and per-row `EXISTS` on a table this
    /// small is the wrong shape for it.
    pub(crate) fn tasks(&self) -> Response {
        let c = self.lock();
        let tasks = match query::all(&c) {
            Ok(t) => t,
            Err(e) => return Response::text(500, &e),
        };
        let blocked = query::blocked_ids(&c).unwrap_or_default();
        let notes = query::note_counts(&c).unwrap_or_default();
        let needs = query::needs_operator(&c).unwrap_or(0);

        let items: Vec<String> = tasks
            .iter()
            .map(|t| task_json(t, blocked.contains(&t.id), *notes.get(&t.id).unwrap_or(&0)))
            .collect();

        // Repos and scopes the board can filter by, taken from the data rather than a
        // hardcoded list — a filter offering a repo with no tasks is a dead end.
        let mut repos: Vec<&str> = tasks.iter().filter_map(|t| t.repo.as_deref()).collect();
        repos.sort_unstable();
        repos.dedup();
        let mut owners: Vec<&str> = tasks
            .iter()
            .filter_map(|t| t.claimed_by.as_deref())
            .collect();
        owners.sort_unstable();
        owners.dedup();

        Response::json(format!(
            "{{\"tasks\":[{}],\"needs_operator\":{needs},\"repos\":[{}],\"owners\":[{}]}}",
            items.join(","),
            repos.iter().map(|r| jstr(r)).collect::<Vec<_>>().join(","),
            owners.iter().map(|o| jstr(o)).collect::<Vec<_>>().join(","),
        ))
    }

    /// One task, with the full log — what the detail panel opens.
    pub(crate) fn task_one(&self, req: &Request) -> Response {
        let Some(id) = req.query.get("id") else {
            return Response::text(400, "missing id");
        };
        let c = self.lock();
        let t = match store::get(&c, id) {
            Ok(Some(t)) => t,
            Ok(None) => return Response::not_found(),
            Err(e) => return Response::text(500, &e),
        };
        let blocked = query::is_blocked(&c, id).unwrap_or(false);
        let notes: Vec<String> = query::notes(&c, id)
            .unwrap_or_default()
            .iter()
            .map(|n| {
                format!(
                    "{{\"ts\":{},\"author\":{},\"kind\":{},\"text\":{}}}",
                    jstr(&n.ts),
                    jstr(&n.author),
                    jstr(&n.kind),
                    jstr(&n.text)
                )
            })
            .collect();
        let deps: Vec<String> = query::deps(&c, id)
            .unwrap_or_default()
            .iter()
            .map(|(did, dstate, dtitle)| {
                format!(
                    "{{\"id\":{},\"state\":{},\"title\":{}}}",
                    jstr(did),
                    jstr(dstate.as_str()),
                    jstr(dtitle)
                )
            })
            .collect();
        Response::json(format!(
            "{{\"task\":{},\"notes\":[{}],\"deps\":[{}]}}",
            task_json(&t, blocked, notes.len() as i64),
            notes.join(","),
            deps.join(",")
        ))
    }

    pub(crate) fn task_create(&self, req: &Request) -> Response {
        let Some(title) = field(&req.body, "title").filter(|t| !t.trim().is_empty()) else {
            return Response::text(400, "a task needs a title");
        };
        let scope = field(&req.body, "scope").unwrap_or_else(|| "global".into());
        let opt = |k: &str| field(&req.body, k).filter(|v| !v.trim().is_empty());
        let n = paos_tasks::model::NewTask {
            title: title.trim().to_string(),
            body: opt("body"),
            scope,
            org: opt("org"),
            repo: opt("repo"),
            parent_id: opt("parent_id"),
            priority: opt("priority").and_then(|p| p.parse().ok()).unwrap_or(2),
            // Created from the dashboard means created by the operator. That is what
            // decides who may close it, so it is not a cosmetic label.
            origin: Origin::Operator,
            created_by: "operator".into(),
            room: opt("room"),
            start_ready: opt("start_ready").is_some(),
        };
        let c = self.lock();
        match store::create(&c, &n, &now_iso()) {
            Ok(id) => Response::json(format!("{{\"ok\":true,\"id\":{}}}", jstr(&id))),
            Err(e) => Response::text(400, &e),
        }
    }

    /// Move a card. The server is the authority on whether the move is legal — the board
    /// animates optimistically and rolls back on a non-200.
    pub(crate) fn task_state(&self, req: &Request) -> Response {
        let Some(id) = field(&req.body, "id") else {
            return Response::text(400, "missing id");
        };
        let Some(to) = field(&req.body, "to").and_then(|s| State::parse(&s)) else {
            return Response::text(400, "unknown state");
        };
        let c = self.lock();
        match store::set_state(&c, &id, to, &store::Actor::Operator, &now_iso()) {
            Ok(()) => Response::json("{\"ok\":true}".into()),
            // 409, not 500: a refused transition is an answer, and the board needs to
            // tell them apart to know whether to show the reason or a generic failure.
            Err(e) => Response::text(409, &e),
        }
    }

    /// Comment on a task, optionally waking whoever is working it.
    ///
    /// The note is written FIRST and unconditionally. A comment must never be lost
    /// because the wake failed — a board you can only watch is a status page, and the
    /// point of this route is that it is not one.
    pub(crate) fn task_note(&self, req: &Request) -> Response {
        let Some(id) = field(&req.body, "id") else {
            return Response::text(400, "missing id");
        };
        let Some(text) = field(&req.body, "text").filter(|t| !t.trim().is_empty()) else {
            return Response::text(400, "empty comment");
        };
        let (owner, room) = {
            let c = self.lock();
            match store::get(&c, &id) {
                Ok(Some(t)) => {
                    if let Err(e) =
                        store::note(&c, &id, "operator", "comment", text.trim(), &now_iso())
                    {
                        return Response::text(500, &e);
                    }
                    (t.claimed_by.clone(), t.room.clone())
                }
                Ok(None) => return Response::not_found(),
                Err(e) => return Response::text(500, &e),
            }
        };
        if field(&req.body, "wake").is_none() {
            return Response::json("{\"ok\":true,\"delivered\":false}".into());
        }
        let Some(owner) = owner else {
            return Response::json(
                "{\"ok\":true,\"delivered\":false,\"why\":\"nobody is holding this task\"}".into(),
            );
        };
        // The note lives on the task, so the wake only has to be a pointer to it.
        let head: String = text.trim().chars().take(80).collect();
        let body = format!("task {id}: {head}");
        let room = room.unwrap_or_else(|| "lobby".into());
        match crate::run_skill(&["bus", "wake", &format!("@{owner}"), &body, "--room", &room]) {
            Ok(_) => Response::json("{\"ok\":true,\"delivered\":true}".into()),
            Err(e) => Response::json(format!(
                "{{\"ok\":true,\"delivered\":false,\"why\":{}}}",
                jstr(&e)
            )),
        }
    }

    pub(crate) fn task_grant(&self, req: &Request) -> Response {
        let Some(id) = field(&req.body, "id") else {
            return Response::text(400, "missing id");
        };
        let c = self.lock();
        match store::grant_close(&c, &id, &now_iso()) {
            Ok(()) => Response::json("{\"ok\":true}".into()),
            Err(e) => Response::text(400, &e),
        }
    }

    pub(crate) fn task_dep(&self, req: &Request) -> Response {
        let (Some(id), Some(dep)) = (field(&req.body, "id"), field(&req.body, "depends_on")) else {
            return Response::text(400, "missing id or depends_on");
        };
        let remove = field(&req.body, "remove").is_some();
        let c = self.lock();
        let r = if remove {
            store::dep_rm(&c, &id, &dep)
        } else {
            store::dep_add(&c, &id, &dep, &now_iso())
        };
        match r {
            Ok(()) => Response::json("{\"ok\":true}".into()),
            Err(e) => Response::text(409, &e),
        }
    }
}

fn task_json(t: &Task, blocked: bool, notes: i64) -> String {
    format!(
        "{{\"id\":{},\"title\":{},\"body\":{},\"state\":{},\"priority\":{},\"scope\":{},\
         \"org\":{},\"repo\":{},\"parent_id\":{},\"origin\":{},\"created_by\":{},\
         \"claimed_by\":{},\"last_owner\":{},\"orphaned\":{},\"close_grant\":{},\"room\":{},\
         \"blocked\":{},\"notes\":{},\"unowned\":{},\"rescue\":{},\"created_ts\":{},\
         \"updated_ts\":{}}}",
        jstr(&t.id),
        jstr(&t.title),
        ostr(t.body.as_deref()),
        jstr(t.state.as_str()),
        t.priority,
        jstr(&t.scope),
        ostr(t.org.as_deref()),
        ostr(t.repo.as_deref()),
        ostr(t.parent_id.as_deref()),
        jstr(t.origin.as_str()),
        jstr(&t.created_by),
        ostr(t.claimed_by.as_deref()),
        ostr(t.last_owner.as_deref()),
        t.orphaned,
        t.close_grant,
        ostr(t.room.as_deref()),
        blocked,
        notes,
        t.is_unowned(),
        t.is_rescue(),
        jstr(&t.created_ts),
        jstr(&t.updated_ts),
    )
}

/// A quoted, escaped JSON string.
///
/// Uses the crate's own `esc` rather than serde_json, which paos-web deliberately does
/// not depend on. Task titles and note text are arbitrary strings written by other
/// sessions, so a quote or a newline in one must not be able to break the payload —
/// which is exactly what `esc` exists and is tested for.
fn jstr(s: &str) -> String {
    format!("\"{}\"", crate::http::esc(s))
}

fn ostr(s: Option<&str>) -> String {
    s.map(jstr).unwrap_or_else(|| "null".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn test_web() -> Web {
        let c = paos_store::open_in_memory().unwrap();
        paos_memory::ensure_schema(&c).unwrap();
        Web {
            conn: Arc::new(Mutex::new(c)),
            embedder: Arc::new(paos_memory::HashEmbedder::new(64)),
        }
    }

    fn get(path: &str) -> Request {
        let (p, q) = match path.split_once('?') {
            Some((p, q)) => (p, q),
            None => (path, ""),
        };
        let mut query = std::collections::HashMap::new();
        for pair in q.split('&').filter(|s| !s.is_empty()) {
            if let Some((k, v)) = pair.split_once('=') {
                query.insert(k.to_string(), v.to_string());
            }
        }
        Request {
            method: "GET".into(),
            path: p.into(),
            query,
            body: String::new(),
            origin: None,
            fetch_site: None,
        }
    }

    /// Same-origin POST: no browser provenance headers, which is what the CLI and the
    /// dashboard's own fetch both look like.
    fn post(path: &str, body: &str) -> Request {
        Request {
            method: "POST".into(),
            path: path.into(),
            query: Default::default(),
            body: body.into(),
            origin: None,
            fetch_site: None,
        }
    }

    fn new_task(title: &str) -> paos_tasks::model::NewTask {
        paos_tasks::model::NewTask {
            title: title.into(),
            body: None,
            scope: "global".into(),
            org: None,
            repo: None,
            parent_id: None,
            priority: 2,
            origin: Origin::Session,
            created_by: "swift-otter".into(),
            room: None,
            start_ready: false,
        }
    }

    fn seed(w: &Web, title: &str) -> String {
        let c = w.lock();
        store::create(&c, &new_task(title), &now_iso()).unwrap()
    }

    #[test]
    fn the_board_payload_carries_the_derived_blocked_flag() {
        let w = test_web();
        let (a, b) = (seed(&w, "first"), seed(&w, "second"));
        {
            let c = w.lock();
            store::dep_add(&c, &b, &a, &now_iso()).unwrap();
        }
        let body = String::from_utf8(w.route(&get("/api/tasks")).body).unwrap();
        assert!(body.contains("\"blocked\":true"), "{body}");
        assert!(body.contains("\"blocked\":false"));
    }

    #[test]
    fn a_cross_origin_post_is_refused_like_every_other_write() {
        // The board's writes are as authoritative as /api/answer — they move work and
        // grant close-authority — so they must sit behind the same CSRF guard rather than
        // quietly being the exception.
        let w = test_web();
        let mut r = post("/api/task/state", "id=t-aaaaaa&to=done");
        r.origin = Some("https://evil.example".into());
        assert_eq!(w.route(&r).status, 403);
        let mut r = post("/api/task/grant", "id=t-aaaaaa");
        r.fetch_site = Some("cross-site".into());
        assert_eq!(w.route(&r).status, 403);
    }

    #[test]
    fn the_board_speaks_as_the_operator_so_it_can_approve_review_items() {
        // Pinning WHICH actor the dashboard is. If it spoke as a session, every operator
        // task would be unclosable from the UI; if `may_close` were skipped entirely,
        // the approval gate would exist only in the CLI.
        let w = test_web();
        let id = {
            let c = w.lock();
            let mut n = new_task("operator task");
            n.origin = Origin::Operator;
            store::create(&c, &n, &now_iso()).unwrap()
        };
        let r = w.route(&post("/api/task/state", &format!("id={id}&to=done")));
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    }

    #[test]
    fn a_refused_transition_answers_409_with_the_reason() {
        let w = test_web();
        let id = seed(&w, "x");
        {
            let c = w.lock();
            store::set_state(&c, &id, State::Dropped, &store::Actor::Operator, &now_iso())
                .unwrap();
        }
        let r = w.route(&post("/api/task/state", &format!("id={id}&to=ready")));
        assert_eq!(r.status, 409);
        assert!(String::from_utf8_lossy(&r.body).contains("dropped"));
    }

    #[test]
    fn a_comment_on_an_unowned_task_is_still_recorded_and_reports_no_delivery() {
        let w = test_web();
        let id = seed(&w, "unowned");
        let r = w.route(&post("/api/task/note", &format!("id={id}&text=hello&wake=1")));
        assert_eq!(r.status, 200);
        let body = String::from_utf8(r.body).unwrap();
        assert!(body.contains("\"delivered\":false"), "{body}");
        let c = w.lock();
        let n = query::notes(&c, &id).unwrap();
        assert!(
            n.iter().any(|x| x.text == "hello"),
            "the comment must survive even when nobody could be woken"
        );
    }

    #[test]
    fn the_badge_counts_what_needs_the_operator() {
        let w = test_web();
        let body = String::from_utf8(w.route(&get("/api/tasks")).body).unwrap();
        assert!(body.contains("\"needs_operator\":0"), "{body}");
    }

    #[test]
    fn titles_with_quotes_do_not_break_the_payload() {
        let w = test_web();
        seed(&w, "a \"quoted\" title\nwith a newline");
        let body = String::from_utf8(w.route(&get("/api/tasks")).body).unwrap();
        // The quote and the newline must arrive escaped, not raw — raw would terminate
        // the string early and the whole board would fail to parse in the browser.
        assert!(body.contains(r#"a \"quoted\" title\nwith a newline"#), "{body}");
        assert!(!body.contains("title\nwith"), "a literal newline leaked into the JSON");
    }

    #[test]
    fn the_board_assets_are_served_from_the_binary() {
        let w = test_web();
        for p in ["/tasks.css", "/tasks.js"] {
            let r = w.route(&get(p));
            assert_eq!(r.status, 200, "{p}");
            assert!(!r.body.is_empty(), "{p}");
        }
    }
}
