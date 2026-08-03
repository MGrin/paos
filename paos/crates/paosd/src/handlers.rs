//! Request handlers.
//!
//! Handlers **return** a `Response` rather than printing. That is the seam the Python
//! lacked: `bus_facet` printed directly, so its tests had to redirect stdout into a
//! `StringIO` and string-match — which is a large part of why one module needed a
//! 3,149-line test file. Here every handler is a pure function of (state, request).

use crate::{Daemon, VERSION};
use paos_operator as op;
use paos_proto::{write_frame, Request, Response};
use rusqlite::Connection;
use std::io;
use std::os::unix::net::UnixStream;

pub fn dispatch(daemon: &Daemon, req: Request) -> Response {
    match req {
        Request::Ping => Response::ok("pong"),
        Request::Version => Response::ok(format!(
            "paosd v{VERSION} · schema v{}",
            paos_store::SCHEMA_VERSION
        )),
        Request::Whoami { session_id } => {
            // Lock only for the duration of the query. A poisoned mutex means another
            // handler panicked mid-request; recover the guard rather than cascading the
            // panic into every subsequent request and taking the daemon down.
            let guard = match daemon.conn.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            whoami(&guard, session_id.as_deref())
        }
        Request::Send { room, sender, target, text, urgent, ambient } => {
            send(daemon, &room, &sender, &target, &text, urgent, ambient)
        }
        // Handled by `listen()` on its own long-lived connection; reaching here means a
        // caller sent Listen down a request/reply path.
        Request::Listen { .. } => {
            Response::err("listen must be sent on a dedicated connection", 2)
        }
        Request::SessionStart { session_id, name, pid } => {
            let g = lock(daemon);
            match paos_presence::session_start(&g, &session_id, &name, pid, &now_iso()) {
                Ok(handle) => Response::ok(handle),
                Err(e) => Response::err(format!("session-start failed: {e}"), 1),
            }
        }
        Request::Heartbeat { session_id, pid } => {
            let g = lock(daemon);
            match paos_presence::heartbeat(&g, &session_id, pid, &now_iso()) {
                Ok(true) => Response::ok("ok"),
                Ok(false) => Response::err("no live session with that id", 3),
                Err(e) => Response::err(format!("heartbeat failed: {e}"), 1),
            }
        }
        Request::SessionEnd { session_id } => {
            let g = lock(daemon);
            match paos_presence::session_end(&g, &session_id, &now_iso()) {
                Ok(true) => Response::ok("retired"),
                Ok(false) => Response::err("no such session", 3),
                Err(e) => Response::err(format!("session-end failed: {e}"), 1),
            }
        }
        Request::Who => {
            let g = lock(daemon);
            // The union, not the push registry alone — see `has_listener`. The roster is
            // how the fleet and the dashboard decide who is deaf, so reporting every
            // flock-armed session as NO made the whole roster misleading.
            let attached = |n: &str| has_listener(daemon, n);
            match paos_presence::live_sessions(&g, &attached) {
                Ok(rows) if rows.is_empty() => Response::ok("(no live sessions)"),
                Ok(rows) => Response::Ok {
                    lines: rows
                        .iter()
                        .map(|s| {
                            format!(
                                "{}\tstatus={}\tlistening={}",
                                s.name,
                                s.status.as_deref().unwrap_or("(idle)"),
                                if s.listening { "yes" } else { "NO" }
                            )
                        })
                        .collect(),
                },
                Err(e) => Response::err(format!("who failed: {e}"), 1),
            }
        }
        Request::Reachable { name } => reachable(daemon, &name),

        // The sweeps report what they DID. Same wording as the Python, so an operator
        // reading either implementation sees the same answer.
        Request::BusReap => {
            let g = lock(daemon);
            match paos_presence::reap_dead(&g, now_epoch()) {
                Ok(names) => Response::ok(format!("reaped {} dead session(s)", names.len())),
                Err(e) => Response::err(format!("reap failed: {e}"), 1),
            }
        }
        Request::BusPrune { older_than_min } => {
            let g = lock(daemon);
            match paos_presence::prune_members(&g, now_epoch(), older_than_min) {
                Ok(stale) if stale.is_empty() => Response::ok("(nothing to prune)"),
                Ok(stale) => {
                    let mut lines = vec![format!(
                        "pruned {} stale member(s) (older than {}m):", stale.len(), older_than_min)];
                    lines.extend(stale.iter().map(|(room, name)| format!("  {name}@{room}")));
                    Response::Ok { lines }
                }
                Err(e) => Response::err(format!("prune failed: {e}"), 1),
            }
        }
        Request::BusPruneRooms => {
            let g = lock(daemon);
            let swept = paos_presence::prune_rooms(&g, now_epoch());
            let ghosts = paos_presence::purge_orphan_members(&g, now_epoch());
            match (swept, ghosts) {
                (Ok((closed, purged)), Ok(ghosts)) => Response::ok(format!(
                    "room GC: closed={closed} purged={purged} ghost-members={ghosts}")),
                (Err(e), _) | (_, Err(e)) => Response::err(format!("room GC failed: {e}"), 1),
            }
        }

        Request::Remember { tier, origin, text, dataset } => {
            if text.trim().is_empty() {
                return Response::err("refusing to store an empty fact", 1);
            }
            // The configured global is read under the same lock the write will take, so
            // a `paos config set` racing this cannot land between the two.
            let g = lock(daemon);
            let global = paos_memory::scope::global_dataset(&g);
            let dataset = match target_dataset(&tier, origin.as_deref(), dataset.as_deref(),
                                               &global) {
                Ok(d) => d,
                Err(r) => return r,
            };
            match paos_memory::remember(&g, daemon.embedder.as_ref(), &dataset, text.trim(), &now_iso()) {
                Ok(id) => Response::ok(format!("stored in {dataset} ({id})")),
                Err(e) => Response::err(format!("remember failed: {e}"), 1),
            }
        }

        Request::Recall { origin, query, top_k, dataset } => {
            // An explicit dataset REPLACES the derived scopes rather than adding to them:
            // the caller asked for one tier, and silently also searching three others
            // would make the flag look like it did nothing.
            let g = lock(daemon);
            let scopes = match dataset.filter(|d| !d.trim().is_empty()) {
                Some(d) => vec![d],
                None => {
                    let parsed = origin.as_deref().and_then(paos_memory::scope::parse_origin);
                    // The CONFIGURED global, not the compiled-in default: a machine that
                    // predates the neutral default keeps its own dataset through this row,
                    // and reaching past it would search a dataset with nothing in it.
                    paos_memory::scope::recall_scopes(
                        parsed.as_ref(), &paos_memory::scope::global_dataset(&g))
                }
            };
            match paos_memory::recall(&g, daemon.embedder.as_ref(), &scopes, &query, top_k) {
                Ok(hits) if hits.is_empty() => Response::ok("(no results)"),
                Ok(hits) => Response::Ok {
                    lines: hits.iter().map(|h| format!(
                        "- [{} {:.2}] {}",
                        &h.memory.created_ts.chars().take(10).collect::<String>(),
                        h.score, h.memory.text)).collect(),
                },
                Err(e) => Response::err(format!("recall failed: {e}"), 1),
            }
        }

        Request::Event { kind, summary, session, reference, data } => {
            if kind.trim().is_empty() {
                return Response::err("event needs a kind", 2);
            }
            let g = lock(daemon);
            match g.execute(
                "INSERT INTO events(ts, kind, session, summary, ref, data) \
                 VALUES(?1,?2,?3,?4,?5,?6)",
                rusqlite::params![now_iso(), kind, session, summary, reference, data],
            ) {
                Ok(_) => Response::ok(format!("recorded {kind}")),
                Err(e) => Response::err(format!("event write failed: {e}"), 1),
            }
        }
        // ---- the shared work queue ----------------------------------------------
        Request::TaskCreate { title, body, scope, org, repo, parent_id, priority, origin,
                              created_by, room, start_ready } => {
            let Some(origin) = paos_tasks::model::Origin::parse(&origin) else {
                return Response::err("origin must be operator|session", 2);
            };
            let n = paos_tasks::model::NewTask {
                title, body, scope, org, repo, parent_id, priority, origin, created_by,
                room, start_ready,
            };
            let g = lock(daemon);
            match paos_tasks::store::create(&g, &n, &now_iso()) {
                // The id alone, so `paos task create ... | xargs paos task claim` works.
                Ok(id) => Response::ok(id),
                Err(e) => Response::err(e, 1),
            }
        }
        Request::TaskClaim { id, session } => {
            let mut g = lock(daemon);
            match paos_tasks::store::claim(&mut g, &id, &session, &now_iso()) {
                Ok(paos_tasks::store::ClaimOutcome::Claimed) => {
                    Response::ok(format!("claimed {id}"))
                }
                // Losing a race is a real answer, not an infrastructure failure — but it
                // is still an error, because the caller must not start work.
                Ok(paos_tasks::store::ClaimOutcome::Lost { holder }) => Response::err(
                    match holder {
                        Some(h) => format!("lost the race: {id} is held by {h}"),
                        None => format!("{id} is not claimable in its current state"),
                    },
                    1,
                ),
                Err(e) => Response::err(e, 1),
            }
        }
        Request::TaskRelease { id, session } => {
            let g = lock(daemon);
            match paos_tasks::store::release(&g, &id, &session, &now_iso()) {
                Ok(()) => Response::ok(format!("released {id}")),
                Err(e) => Response::err(e, 1),
            }
        }
        Request::TaskState { id, to, actor } => {
            let Some(to) = paos_tasks::model::State::parse(&to) else {
                return Response::err("unknown state", 2);
            };
            let a = if actor == "operator" {
                paos_tasks::store::Actor::Operator
            } else {
                paos_tasks::store::Actor::Session(&actor)
            };
            let g = lock(daemon);
            match paos_tasks::store::set_state(&g, &id, to, &a, &now_iso()) {
                Ok(()) => Response::ok(format!("{id} → {}", to.as_str())),
                Err(e) => Response::err(e, 1),
            }
        }
        Request::TaskNote { id, author, text } => {
            if text.trim().is_empty() {
                return Response::err("a note needs text", 2);
            }
            let g = lock(daemon);
            match paos_tasks::store::get(&g, &id) {
                Ok(None) => Response::err(format!("no such task: {id}"), 1),
                Err(e) => Response::err(e, 1),
                Ok(Some(_)) => match paos_tasks::store::note(&g, &id, &author, "note", &text,
                                                             &now_iso()) {
                    Ok(()) => Response::ok(format!("noted on {id}")),
                    Err(e) => Response::err(e, 1),
                },
            }
        }
        Request::TaskGrant { id } => {
            let g = lock(daemon);
            match paos_tasks::store::grant_close(&g, &id, &now_iso()) {
                Ok(()) => Response::ok(format!("close-authority granted on {id}")),
                Err(e) => Response::err(e, 1),
            }
        }
        Request::TaskDep { id, depends_on, remove } => {
            let g = lock(daemon);
            let r = if remove {
                paos_tasks::store::dep_rm(&g, &id, &depends_on)
            } else {
                paos_tasks::store::dep_add(&g, &id, &depends_on, &now_iso())
            };
            match r {
                Ok(()) => Response::ok(if remove {
                    format!("{id} no longer depends on {depends_on}")
                } else {
                    format!("{id} depends on {depends_on}")
                }),
                Err(e) => Response::err(e, 1),
            }
        }

        Request::OperatorAsk { session, question, options } => {
            if question.trim().is_empty() {
                return Response::err("ask needs a question", 2);
            }
            let g = lock(daemon);
            match op::ask(&g, &session, question.trim(), options.as_deref(), &now_iso()) {
                Ok(id) => Response::ok(format!("{id}")),
                Err(e) => Response::err(format!("ask failed: {e}"), 1),
            }
        }
        Request::OperatorAnswer { id, text } => {
            let mut g = lock(daemon);
            match op::answer(&mut g, id, &text, &now_iso()) {
                Ok(true) => Response::ok(format!("answered #{id}")),
                Ok(false) => Response::ok(format!("no open escalation #{id}")),
                Err(e) => Response::err(format!("answer failed: {e}"), 1),
            }
        }
        Request::OperatorResolve { id } => {
            let g = lock(daemon);
            match op::resolve(&g, id, &now_iso()) {
                Ok(true) => Response::ok(format!("resolved #{id}")),
                Ok(false) => Response::ok(format!("no open escalation #{id}")),
                Err(e) => Response::err(format!("resolve failed: {e}"), 1),
            }
        }
        Request::OperatorPark { session, note } => {
            let g = lock(daemon);
            match op::park(&g, &session, &note, &now_iso()) {
                Ok(_) => Response::ok("parked"),
                Err(e) => Response::err(format!("park failed: {e}"), 1),
            }
        }
        Request::OperatorResolvePark { id } => {
            let g = lock(daemon);
            match op::resolve_park(&g, id) {
                Ok(true) => Response::ok(format!("resolved park #{id}")),
                Ok(false) => Response::ok(format!("no open park #{id}")),
                Err(e) => Response::err(format!("resolve-park failed: {e}"), 1),
            }
        }
        Request::OperatorSay { session, text } => {
            if text.trim().is_empty() {
                return Response::err("say needs a body", 2);
            }
            let g = lock(daemon);
            match op::enqueue_say(&g, &session, text.trim(), &now_iso()) {
                Ok(_) => Response::ok("queued (paosd delivers within ~2s)"),
                Err(e) => Response::err(format!("say failed: {e}"), 1),
            }
        }
        Request::OperatorSend { text } => {
            let g = lock(daemon);
            match op::record_operator_message(&g, &text, &now_iso()) {
                Ok(_) => Response::ok("sent"),
                Err(e) => Response::err(format!("send failed: {e}"), 1),
            }
        }
        Request::OperatorSetMode { mode, by } => {
            let Some(m) = op::Mode::parse(&mode) else {
                return Response::err(
                    format!("unknown mode {mode:?} — use attended, autonomous or away"), 2);
            };
            let g = lock(daemon);
            match op::set_mode(&g, m, &by, &now_iso()) {
                // Broadcast on a REAL change, exactly as the Telegram path does. The
                // Python's set_mode posted to the lobby too; writing only the table would
                // have made `paos operator mode away` silently different from `/away`,
                // and the fleet would not learn the channel had opened.
                //
                // Ambient: the banner cost one full turn per session per toggle when it
                // woke everyone — measured at 9 lobby members with ~17.6M tokens between
                // them.
                Ok(changed) => {
                    if changed {
                        let _ = crate::bridge::post_as_operator(
                            &g, "lobby", "@all",
                            &format!("⚙ operator mode → {mode}"), true);
                    }
                    Response::ok(format!("operator mode: {mode}"))
                }
                Err(e) => Response::err(format!("mode failed: {e}"), 1),
            }
        }
        Request::EventPrune { days } => {
            let g = lock(daemon);
            // Computed in SQL so the cutoff uses the same clock as the rows.
            match g.execute(
                "DELETE FROM events WHERE ts < strftime('%Y-%m-%dT%H:%M:%SZ', \
                 'now', ?1)", [format!("-{days} days")]) {
                Ok(n) => Response::ok(format!("pruned {n} event(s)")),
                Err(e) => Response::err(format!("prune failed: {e}"), 1),
            }
        }
        Request::StandupBrief { side, covers_from, covers_to, body } => {
            let g = lock(daemon);
            // One draft per side: regenerating replaces rather than stacking, so `show`
            // cannot present a stale brief as the current one.
            let r = g.execute("DELETE FROM standup_briefs WHERE side=?1 AND status='draft'",
                              [&side])
                .and_then(|_| g.execute(
                    "INSERT INTO standup_briefs(ts, side, covers_from, covers_to, body, \
                     model, status) VALUES(?1,?2,?3,?4,?5,'claude-code','draft')",
                    rusqlite::params![now_iso(), side, covers_from, covers_to, body]));
            match r {
                Ok(_) => Response::ok(format!("saved {side} brief")),
                Err(e) => Response::err(format!("brief save failed: {e}"), 1),
            }
        }
        Request::StandupReported { side } => {
            let g = lock(daemon);
            // Advance to the draft's covers_to, NOT to wall-clock now: work logged while
            // the brief was being written would otherwise fall in the gap and never
            // appear in any brief.
            let to: String = g.query_row(
                "SELECT covers_to FROM standup_briefs WHERE side=?1 AND status='draft' \
                 ORDER BY id DESC LIMIT 1", [&side], |r| r.get(0))
                .unwrap_or_else(|_| now_iso());
            let r = g.execute(
                "UPDATE standup_briefs SET status='reported' WHERE side=?1 AND status='draft'",
                [&side])
                .and_then(|_| g.execute(
                    "INSERT INTO standup_watermark(side, reported_ts) VALUES(?1,?2) \
                     ON CONFLICT(side) DO UPDATE SET reported_ts=excluded.reported_ts",
                    rusqlite::params![side, to]));
            match r {
                Ok(_) => Response::ok(format!("{side} reported through {to}")),
                Err(e) => Response::err(format!("reported failed: {e}"), 1),
            }
        }
        Request::ConfigGet => {
            let g = lock(daemon);
            match g.prepare("SELECT key, value FROM paos_config ORDER BY key")
                .and_then(|mut s| {
                    s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                        .map(|rows| rows.flatten().collect::<Vec<_>>())
                }) {
                Ok(pairs) => Response::Ok {
                    lines: pairs.into_iter().map(|(k, v)| format!("{k}={v}")).collect(),
                },
                Err(e) => Response::err(format!("config read failed: {e}"), 1),
            }
        }
        Request::ConfigSet { key, value } => {
            if key.trim().is_empty() {
                return Response::err("config set needs a key", 2);
            }
            let g = lock(daemon);
            match g.execute(
                "INSERT INTO paos_config(key,value,updated_ts) VALUES(?1,?2,?3) \
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value, \
                 updated_ts=excluded.updated_ts",
                rusqlite::params![key, value, now_iso()],
            ) {
                Ok(_) => Response::ok(format!("set {key}={value}")),
                Err(e) => Response::err(format!("config write failed: {e}"), 1),
            }
        }
        Request::ConfigSources => {
            let g = lock(daemon);
            let from_db = |k: &str| -> bool {
                g.query_row("SELECT value FROM paos_config WHERE key=?1", [k],
                            |r| r.get::<_, String>(0))
                    .ok()
                    .is_some_and(|v: String| !v.trim().is_empty())
            };
            // The same two files main() reads, in the same order.
            let home = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default());
            let env_path = [home.join(".paos/.env"), home.join(".claude/skills/paos/.env")]
                .into_iter()
                .find(|p| p.exists())
                .unwrap_or_else(|| home.join(".paos/.env"));
            let tg = paos_operator::telegram::Config::from_db_or_env(&g, &env_path);
            let from_env = |k: &str| -> bool {
                match (&tg, k) {
                    (Some(c), "telegram_bot_token") => !c.token.is_empty(),
                    (Some(c), "telegram_chat_id") => !c.chat_id.is_empty(),
                    (Some(c), "telegram_allowed_user_id") => c.allowed_user_id.is_some(),
                    (Some(c), "telegram_operator_username") => c.operator_username.is_some(),
                    _ => false,
                }
            };
            let lines = ["telegram_bot_token", "telegram_chat_id",
                         "telegram_allowed_user_id", "telegram_operator_username"]
                .iter()
                .map(|k| {
                    let src = if from_db(k) { "config" }
                              else if from_env(k) { "env" }
                              else { "unset" };
                    format!("{k}={src}")
                })
                .collect();
            Response::Ok { lines }
        }

        Request::SecretStatus { key } => {
            let g = lock(daemon);
            let raw: String = g
                .query_row("SELECT value FROM paos_config WHERE key=?1", [&key], |r| r.get(0))
                .unwrap_or_default();
            // Reads the DAEMON's environment, which is the one the bridge will read the
            // token from — so the answer describes the environment that will actually be
            // used rather than the asking client's.
            let env = |n: &str| std::env::var(n).ok();
            let s = match paos_secrets::status(&raw, &env) {
                paos_secrets::Status::Configured => "configured",
                paos_secrets::Status::Missing => "missing",
                paos_secrets::Status::Unreadable => "unreadable",
            };
            Response::ok(s)
        }

        Request::MemoryHealth => {
            let g = lock(daemon);
            match paos_memory::health::analyse(&g, 500) {
                Ok(rep) => Response::Ok {
                    lines: paos_memory::health::render(&rep).lines().map(str::to_string).collect(),
                },
                Err(e) => Response::err(format!("health failed: {e}"), 1),
            }
        }
        Request::Doctor => {
            let g = lock(daemon);
            Response::Ok { lines: paos_memory::doctor::render(&paos_memory::doctor::run(&g)) }
        }
        Request::Forget { id } => {
            let g = lock(daemon);
            match paos_memory::forget(&g, &id) {
                Ok(true) => Response::ok(format!("forgot {id}")),
                Ok(false) => Response::err(format!("no memory with id {id}"), 3),
                Err(e) => Response::err(format!("forget failed: {e}"), 1),
            }
        }
        Request::Supersede { old_ids, tier, origin, text, dataset } => {
            if text.trim().is_empty() {
                return Response::err("refusing to store an empty fact", 1);
            }
            // The configured global is read under the same lock the write will take, so
            // a `paos config set` racing this cannot land between the two.
            let g = lock(daemon);
            let global = paos_memory::scope::global_dataset(&g);
            let dataset = match target_dataset(&tier, origin.as_deref(), dataset.as_deref(),
                                               &global) {
                Ok(d) => d,
                Err(r) => return r,
            };
            // Store first. If the write fails there is nothing to retire the originals in
            // favour of, and losing a fact to a failed replacement is the outcome worth
            // engineering against.
            let new_id = match paos_memory::remember(
                &g, daemon.embedder.as_ref(), &dataset, text.trim(), &now_iso()) {
                Ok(id) => id,
                Err(e) => return Response::err(
                    format!("remember failed, originals kept: {e}"), 1),
            };
            let mut retired = 0;
            for old in &old_ids {
                match paos_memory::supersede(&g, old, &new_id) {
                    Ok(true) => retired += 1,
                    // Already retired, or never there. The new fact is stored either way,
                    // so the caller's intent holds and there is nothing to redo.
                    Ok(false) => {}
                    Err(e) => return Response::err(
                        format!("stored ({new_id}), but retiring {old} failed: {e}"), 1),
                }
            }
            Response::ok(format!("stored in {dataset} ({new_id}); retired {retired} fact(s)"))
        }
        Request::ProposalAdd {
            kind, dataset, text, scope, target_data_id, rationale, source,
        } => {
            let g = lock(daemon);
            match paos_librarian::queue::add(
                &g,
                &kind,
                &dataset,
                text.as_deref(),
                scope.as_deref(),
                target_data_id.as_deref(),
                rationale.as_deref(),
                source.as_deref(),
                &now_iso(),
            ) {
                Ok(id) => Response::ok(format!("proposal {id}")),
                Err(e) => Response::err(format!("queueing the proposal failed: {e}"), 1),
            }
        }
        Request::ProposalSetStatus { id, status } => {
            if status != "approved" && status != "rejected" {
                return Response::err("status must be approved or rejected", 2);
            }
            let g = lock(daemon);
            match paos_librarian::queue::set_status(&g, id, &status, &now_iso()) {
                // Not an error: the guard on `status='pending'` is what makes a
                // double-approve a no-op instead of a second application.
                Ok(false) => Response::ok(format!("proposal {id} was already resolved")),
                Ok(true) => Response::ok(format!("proposal {id} {status}")),
                Err(e) => Response::err(format!("resolving proposal {id} failed: {e}"), 1),
            }
        }
    }
}

/// Where a fact belongs: an explicit dataset wins, otherwise derive from tier + origin.
///
/// Shared by `Remember` and `Supersede` so a replacement lands in the same place the
/// original would have. An explicit dataset skips derivation entirely — the caller (the
/// review queue) recorded it when the proposal was drafted, and re-deriving from the
/// approving session's cwd would file the fact under the wrong project.
fn target_dataset(tier: &str, origin: Option<&str>, dataset: Option<&str>, global: &str)
    -> Result<String, Response> {
    if let Some(ds) = dataset.map(str::trim).filter(|d| !d.is_empty()) {
        return Ok(ds.to_string());
    }
    let tier = match tier {
        "global" => paos_memory::scope::Tier::Global,
        "org" => paos_memory::scope::Tier::Org,
        "project" => paos_memory::scope::Tier::Project,
        // No default on purpose: an implicit scope is how a repo-specific fact ends up in
        // every session's recall forever.
        other => return Err(Response::err(
            format!("unknown scope {other:?} — use global, org or project"), 2)),
    };
    let parsed = origin.and_then(paos_memory::scope::parse_origin);
    paos_memory::scope::write_scope(tier, parsed.as_ref(), global)
        .map_err(|e| Response::err(e, 2))
}

fn lock(daemon: &Daemon) -> std::sync::MutexGuard<'_, Connection> {
    match daemon.conn.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// Verify and repair reachability — the end-of-turn reflex.
///
/// Liveness here is a **registry lookup**, not an inference. The Python answered it by
/// probing an advisory lock and cross-checking a process table that returns nothing
/// inside the sandbox, so `0 orphans` meant "cannot see" and four sessions in one night
/// each hit a different false-PASS. The daemon holds the listener; it simply knows.
///
/// Repair restores rooms the session was dropped from — but never rooms it *left*, which
/// is why `leave` deletes the history row.
/// Is a listener armed for `name` — by EITHER mechanism?
///
/// There are two, and for months only one of them was consulted here:
///   * an open `Listen` socket, tracked in the push registry;
///   * an flock on `~/.paos/listen/<handle>.lock`, taken by `paos bus wait-joined`.
///
/// Every session on this machine uses the second, because the sandbox they all run in
/// denies unix sockets — 13 flock locks were held by live sessions when this was written,
/// and zero sessions were socket-attached. So consulting only the push registry reported
/// NOT LISTENING for every correctly-armed session, and the documented response to NOT
/// LISTENING is to arm another listener. That is a fleet-wide double-arm loop, and it is
/// the failure `references/wake-loop-incidents.md` is 215 lines about.
///
/// The answer is the UNION, not a choice between them: a socket-attached listener really
/// is listening, so dropping the registry would be wrong in the other direction.
fn has_listener(daemon: &Daemon, name: &str) -> bool {
    has_listener_at(daemon.push.has_listener(name), &paos_store::root(), name)
}

/// Split so a test can point the flock half at a scratch directory instead of setting
/// PAOS_ROOT, which is process-global and races every other test in the binary.
fn has_listener_at(push_attached: bool, root: &std::path::Path, name: &str) -> bool {
    push_attached || paos_bus::readonly::is_listening(root, name)
}

fn reachable(daemon: &Daemon, name: &str) -> Response {
    let g = lock(daemon);
    let now = now_iso();
    let mut lines = Vec::new();

    let joined = paos_bus::joined_rooms(&g, name).unwrap_or_default();
    let prior = paos_presence::prior_rooms(&g, name).unwrap_or_default();
    let mut restored = Vec::new();
    for room in prior.iter() {
        if !joined.contains(room) {
            if paos_presence::join(&g, room, name, &now).is_ok() {
                restored.push(room.clone());
            }
        }
    }
    let rooms = paos_bus::joined_rooms(&g, name).unwrap_or_default();
    lines.push(format!("rooms: {}", if rooms.is_empty() { "(none)".into() } else { rooms.join(", ") }));
    if !restored.is_empty() {
        lines.push(format!("  restored {} dropped room(s): {}", restored.len(), restored.join(", ")));
    }

    if has_listener(daemon, name) {
        lines.push("REACHABLE: rooms ok, listener live.".into());
        Response::Ok { lines }
    } else {
        lines.push("NOT LISTENING — arm one now (Bash run_in_background, no trailing '&'):".into());
        // The verb the wake loop actually documents. `paos listen <handle> <rooms>` is a
        // repair instruction no session runs, so it is not a repair.
        lines.push("    paos bus wait-joined".into());
        Response::Err { message: lines.join("\n"), exit_code: 1 }
    }
}

/// Seconds since the epoch, for the supervisor sweeps.
fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn now_iso() -> String {
    // RFC3339-ish UTC to the second, matching what the Python wrote, so both
    // implementations can read each other's rows during the cutover.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    // civil_from_days (Howard Hinnant), the standard branch-free algorithm.
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
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y, m, d, tod / 3600, (tod % 3600) / 60, tod % 60
    )
}

/// Post a message, then wake exactly the listeners it should.
///
/// The publish happens AFTER the commit, so a woken listener can never observe a
/// message that is not yet durable.
fn send(
    daemon: &Daemon,
    room: &str,
    sender: &str,
    target: &str,
    text: &str,
    urgent: bool,
    ambient: bool,
) -> Response {
    if text.trim().is_empty() {
        // The Python posted blank messages when a shell substitution failed, which
        // reached peers as noise. Refuse loudly instead.
        return Response::err("refusing to send an empty message", 1);
    }
    let ts = now_iso();
    let posted = {
        let mut guard = match daemon.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        paos_bus::post(&mut guard, room, sender, target, text, &ts, urgent, ambient)
    };
    match posted {
        Ok((id, seq)) => {
            let m = paos_bus::Message {
                id,
                room: room.to_string(),
                seq,
                ts,
                sender: sender.to_string(),
                target: target.to_string(),
                text: text.to_string(),
                urgent,
                ambient,
            };
            let woken = daemon.push.publish(&m, daemon.broadcast_wakes);
            Response::ok(format!("sent #{seq} -> {target} ({woken} woken)"))
        }
        Err(e) => Response::err(format!("send failed: {e}"), 1),
    }
}

/// Block until something wakes this listener, then write it and return.
///
/// One frame per wake, then the connection closes — matching the existing wake-loop
/// contract where the listener exits and the harness re-invokes the session.
pub fn listen(
    daemon: &Daemon,
    stream: &mut UnixStream,
    name: &str,
    rooms: Vec<String>,
    urgent_only: bool,
) -> io::Result<()> {
    let sub = daemon.push.subscribe(name, rooms, urgent_only);
    // Blocks with no timer and no query. An idle listener costs one parked thread.
    let msg = sub.rx.recv();
    daemon.push.unsubscribe(sub.id);
    let res = match msg {
        Ok(m) => Response::Ok {
            lines: vec![format!("[{}] {} -> {}: {}", m.room, m.sender, m.target, m.text)],
        },
        // The daemon dropped the sender: shutting down. Benign teardown, re-arm.
        Err(_) => Response::err("listener torn down — re-arm", 6),
    };
    write_frame(stream, &res)
}

/// The handle bound to this Claude `session_id`.
///
/// Identity is looked up here rather than derived in the CLI on purpose: the Python CLI
/// shelled out to `git` on every invocation to infer scope, which measured **25.7 ms of
/// a 122.6 ms** command. The daemon already knows, so the client does not have to ask.
fn whoami(conn: &Connection, session_id: Option<&str>) -> Response {
    let Some(sid) = session_id.filter(|s| !s.is_empty()) else {
        return Response::err(
            "no session id — pass --session-id or set CLAUDE_SESSION_ID",
            2,
        );
    };
    let found: rusqlite::Result<String> = conn.query_row(
        "SELECT name FROM sessions WHERE session_id = ?1 AND ended_ts IS NULL",
        [sid],
        |r| r.get(0),
    );
    match found {
        Ok(name) => Response::ok(name),
        Err(rusqlite::Error::QueryReturnedNoRows) => Response::err(
            format!("no live session bound to session_id {sid}"),
            3,
        ),
        Err(e) => Response::err(format!("lookup failed: {e}"), 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn daemon_with(rows: &[(&str, &str, Option<&str>)]) -> Daemon {
        let conn = paos_store::open_in_memory().unwrap();
        for (name, sid, ended) in rows {
            conn.execute(
                "INSERT INTO sessions(name, session_id, updated_ts, ended_ts) \
                 VALUES(?1, ?2, 't', ?3)",
                rusqlite::params![name, sid, ended],
            )
            .unwrap();
        }
        Daemon {
            conn: std::sync::Arc::new(Mutex::new(conn)),
            push: paos_bus::push::Registry::new(),
            broadcast_wakes: false,
            // Tests use the deterministic lexical backend: loading a 123 MB model per
            // test would be slow, and none of these assertions are about embedding.
            embedder: std::sync::Arc::new(paos_memory::HashEmbedder::new(64)),
        }
    }

    #[test]
    fn secret_status_reports_state_and_never_the_value() {
        let d = daemon_with(&[]);
        {
            let g = lock(&d);
            g.execute("INSERT INTO paos_config(key,value,updated_ts) \
                       VALUES('telegram_bot_token','env:PAOS_TEST_TOKEN_UNSET','t')", [])
                .unwrap();
        }
        let r = dispatch(&d, Request::SecretStatus { key: "telegram_bot_token".into() });
        let lines = match r { Response::Ok { lines } => lines, other => panic!("{other:?}") };
        assert_eq!(lines, vec!["missing"]);
    }

    #[test]
    fn secret_status_on_an_unknown_key_is_missing_not_an_error() {
        // The dashboard asks for every secret in the schema, including ones nothing has
        // ever written. An error there would blank the settings page.
        let d = daemon_with(&[]);
        let r = dispatch(&d, Request::SecretStatus { key: "nope".into() });
        let lines = match r { Response::Ok { lines } => lines, other => panic!("{other:?}") };
        assert_eq!(lines, vec!["missing"]);
    }

    #[test]
    fn listening_is_the_union_of_the_socket_registry_and_the_flock() {
        // Only the push registry was consulted here for months. Every session on this
        // machine arms `paos bus wait-joined`, which takes an flock and opens no socket
        // (the sandbox denies sockets), so all 13 live sessions read as NOT LISTENING —
        // and the documented response to NOT LISTENING is to arm another listener.
        //
        // The union is the fix in both directions: a socket-attached listener is also
        // genuinely listening, so the registry cannot simply be dropped.
        use std::os::unix::io::AsRawFd;
        let base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
        let root = std::path::PathBuf::from(base).join(format!("paos-union-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("listen")).unwrap();

        // Neither mechanism: genuinely not listening.
        assert!(!has_listener_at(false, &root, "flock-otter"));
        // Socket only (the old notion) still counts.
        assert!(has_listener_at(true, &root, "flock-otter"));

        // Flock only — the case every real session is in, and the one that was wrong.
        let lock = root.join("listen").join("flock-otter.lock");
        std::fs::write(&lock, "4242").unwrap();
        let held = std::fs::OpenOptions::new().read(true).write(true).open(&lock).unwrap();
        // flock(LOCK_EX|LOCK_NB) via the same path the probe uses.
        assert!(paos_bus::readonly::listener_pid(&lock).is_none(), "not held yet");
        unsafe {
            extern "C" { fn flock(fd: i32, op: i32) -> i32; }
            assert_eq!(flock(held.as_raw_fd(), 2 | 4), 0);
        }
        assert!(has_listener_at(false, &root, "flock-otter"),
                "a flock-armed session must not read as deaf");
    }

    #[test]
    fn ping_does_not_touch_the_database() {
        // Liveness must be answerable even if the DB is wedged.
        let d = daemon_with(&[]);
        assert_eq!(dispatch(&d, Request::Ping), Response::ok("pong"));
    }

    #[test]
    fn version_reports_the_schema() {
        let d = daemon_with(&[]);
        match dispatch(&d, Request::Version) {
            Response::Ok { lines } => {
                assert!(lines[0].contains("schema v"), "got {lines:?}");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn pruning_removes_old_events_and_keeps_recent_ones() {
        let d = daemon_with(&[]);
        {
            let g = d.conn.lock().unwrap();
            for ts in ["2020-01-01T00:00:00Z", "2999-01-01T00:00:00Z"] {
                g.execute("INSERT INTO events(ts, kind, summary) VALUES(?1,'k','s')",
                          [ts]).unwrap();
            }
        }
        match dispatch(&d, Request::EventPrune { days: 30 }) {
            Response::Ok { lines } => assert!(lines[0].contains("pruned 1"), "{lines:?}"),
            other => panic!("expected Ok, got {other:?}"),
        }
        let g = d.conn.lock().unwrap();
        let n: i64 = g.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "the recent event must survive");
    }

    #[test]
    fn setting_the_mode_from_the_cli_broadcasts_to_the_fleet() {
        // The Python's set_mode posted to the lobby. Writing only the table would make
        // `paos operator mode away` silently different from Telegram's `/away`, and the
        // fleet would never learn the channel had opened.
        let d = daemon_with(&[]);
        let r = dispatch(&d, Request::OperatorSetMode {
            mode: "away".into(), by: "test".into() });
        assert!(matches!(r, Response::Ok { .. }));
        let g = d.conn.lock().unwrap();
        let (room, text, ambient): (String, String, i64) = g.query_row(
            "SELECT room, text, ambient FROM messages ORDER BY id DESC LIMIT 1",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!(room, "lobby");
        assert!(text.contains("operator mode"), "{text}");
        // Ambient: waking every session for a banner cost one full turn each per toggle.
        assert_eq!(ambient, 1, "a mode banner must not wake the fleet");
    }

    #[test]
    fn setting_the_mode_to_what_it_already_is_does_not_rebroadcast() {
        let d = daemon_with(&[]);
        dispatch(&d, Request::OperatorSetMode { mode: "away".into(), by: "t".into() });
        dispatch(&d, Request::OperatorSetMode { mode: "away".into(), by: "t".into() });
        let g = d.conn.lock().unwrap();
        let n: i64 = g.query_row(
            "SELECT COUNT(*) FROM messages WHERE text LIKE '%operator mode%'", [],
            |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "a no-op set must stay silent");
    }

    #[test]
    fn an_unknown_mode_is_refused_rather_than_stored() {
        let d = daemon_with(&[]);
        match dispatch(&d, Request::OperatorSetMode { mode: "afk".into(), by: "t".into() }) {
            Response::Err { exit_code, .. } => assert_eq!(exit_code, 2),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn an_escalation_asked_over_the_socket_is_answerable_and_attributed() {
        let d = daemon_with(&[]);
        let id: i64 = match dispatch(&d, Request::OperatorAsk {
            session: "quiet-otter".into(), question: "which currency?".into(),
            options: None }) {
            Response::Ok { lines } => lines[0].parse().unwrap(),
            other => panic!("expected Ok, got {other:?}"),
        };
        let g = d.conn.lock().unwrap();
        let (session, status): (String, String) = g.query_row(
            "SELECT session, status FROM escalations WHERE id=?1", [id],
            |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(session, "quiet-otter", "the answer is routed back by this name");
        assert_eq!(status, "open");
    }

    #[test]
    fn an_empty_question_is_refused() {
        // An empty escalation reaches the operator as a blank prompt they cannot answer.
        let d = daemon_with(&[]);
        match dispatch(&d, Request::OperatorAsk {
            session: "s".into(), question: "   ".into(), options: None }) {
            Response::Err { exit_code, .. } => assert_eq!(exit_code, 2),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn whoami_resolves_a_live_session() {
        let d = daemon_with(&[("swift-otter", "sid-1", None)]);
        let res = dispatch(&d, Request::Whoami { session_id: Some("sid-1".into()) });
        assert_eq!(res, Response::ok("swift-otter"));
    }

    #[test]
    fn whoami_ignores_an_archived_session() {
        // A retired session's handle must not come back; handles are per-session.
        let d = daemon_with(&[("old-otter", "sid-1", Some("2026-07-01T00:00:00Z"))]);
        match dispatch(&d, Request::Whoami { session_id: Some("sid-1".into()) }) {
            Response::Err { exit_code, .. } => assert_eq!(exit_code, 3),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn whoami_without_a_session_id_fails_loudly() {
        // Exiting 0 with no answer is exactly the Python failure mode being removed.
        let d = daemon_with(&[]);
        for sid in [None, Some(String::new())] {
            match dispatch(&d, Request::Whoami { session_id: sid }) {
                Response::Err { exit_code, .. } => assert_eq!(exit_code, 2),
                other => panic!("expected Err, got {other:?}"),
            }
        }
    }

    #[test]
    fn unknown_session_is_distinguishable_from_a_missing_argument() {
        // Different exit codes so a caller can tell "you forgot an argument" from
        // "that session is gone".
        let d = daemon_with(&[("a", "sid-1", None)]);
        let missing = dispatch(&d, Request::Whoami { session_id: None });
        let unknown = dispatch(&d, Request::Whoami { session_id: Some("nope".into()) });
        match (missing, unknown) {
            (Response::Err { exit_code: a, .. }, Response::Err { exit_code: b, .. }) => {
                assert_ne!(a, b)
            }
            other => panic!("expected two Errs, got {other:?}"),
        }
    }

    #[test]
    fn send_refuses_an_empty_body() {
        // A failed shell substitution used to post a blank message to peers as noise.
        let d = daemon_with(&[]);
        for body in ["", "   ", "\n"] {
            match dispatch(&d, Request::Send {
                room: "lobby".into(), sender: "me".into(), target: "@all".into(),
                text: body.into(), urgent: false, ambient: false,
            }) {
                Response::Err { exit_code, .. } => assert_eq!(exit_code, 1),
                other => panic!("expected Err for {body:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn send_persists_and_allocates_a_seq() {
        let d = daemon_with(&[]);
        let res = dispatch(&d, Request::Send {
            room: "lobby".into(), sender: "me".into(), target: "@all".into(),
            text: "hello".into(), urgent: false, ambient: false,
        });
        match res {
            Response::Ok { lines } => assert!(lines[0].starts_with("sent #1"), "{lines:?}"),
            other => panic!("expected Ok, got {other:?}"),
        }
        let g = d.conn.lock().unwrap();
        let n: i64 = g.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn send_wakes_only_listeners_it_should() {
        let d = daemon_with(&[]);
        let me = d.push.subscribe("me", vec!["lobby".into()], false);
        let you = d.push.subscribe("you", vec!["lobby".into()], false);

        // addressed to me -> wakes me only
        dispatch(&d, Request::Send {
            room: "lobby".into(), sender: "peer".into(), target: "@me".into(),
            text: "yours".into(), urgent: false, ambient: false,
        });
        assert!(me.rx.try_recv().is_ok());
        assert!(you.rx.try_recv().is_err());

        // plain @all broadcast -> wakes nobody
        dispatch(&d, Request::Send {
            room: "lobby".into(), sender: "peer".into(), target: "@all".into(),
            text: "chatter".into(), urgent: false, ambient: false,
        });
        assert!(me.rx.try_recv().is_err());
        assert!(you.rx.try_recv().is_err());
    }

    #[test]
    fn listen_sent_on_the_wrong_path_is_refused_not_ignored() {
        let d = daemon_with(&[]);
        match dispatch(&d, Request::Listen {
            name: "me".into(), rooms: vec!["lobby".into()], urgent_only: false,
        }) {
            Response::Err { exit_code, .. } => assert_eq!(exit_code, 2),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn timestamps_are_iso_utc_matching_the_python_format() {
        // Both implementations read each other's rows during the cutover.
        let ts = now_iso();
        assert_eq!(ts.len(), 20, "{ts}");
        assert!(ts.ends_with('Z') && ts.contains('T'), "{ts}");
        let y: i32 = ts[..4].parse().unwrap();
        assert!((2024..2100).contains(&y), "implausible year in {ts}");
    }
}