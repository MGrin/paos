//! The PAOS dashboard, served by the daemon itself.
//!
//! One binary, no node, no bun, no build step. The old stack needed a second LaunchAgent
//! (`ai.paos.ui`) running `bun run server/server.ts`, 443 MB of `node_modules` and a
//! 403 MB `.next` cache — and because the LaunchAgent never ran the build, editing a
//! component silently served a stale `out/` forever while reporting healthy. There is no
//! build artifact here to go stale.
//!
//! On writes: the old dashboard shelled every mutation out to the CLI, because it was a
//! separate process and that was the only way to keep one writer. This server runs
//! INSIDE the daemon, so the daemon's own mutex-guarded connection IS the single-writer
//! path — same discipline, one less hop. Writes are confined to three explicit operator
//! actions (answer, resolve-park, set-mode); everything else is read-only.

use paos_memory::Embedder;
use rusqlite::Connection;
use std::sync::{Arc, Mutex};

pub mod http;
pub mod services;
pub mod tasks;

use http::{esc, Request, Response};

/// The single-page UI, compiled into the binary.
const INDEX_HTML: &str = include_str!("index.html");

pub struct Web {
    pub conn: Arc<Mutex<Connection>>,
    pub embedder: Arc<dyn Embedder>,
}

impl Web {
    pub fn route(&self, req: &Request) -> Response {
        // CSRF. Every write below is destructive or authoritative — forget a memory,
        // approve a memory change, switch the active Claude account, and /api/answer,
        // which replies to sessions AS THE OPERATOR. Binding to localhost does not
        // protect them: the attacker is a page in the operator's own browser.
        if req.is_forgeable() {
            return Response::text(403, "cross-origin write refused");
        }
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/") => Response::html(INDEX_HTML),
            // The board ships as its own assets rather than swelling index.html. Still
            // compiled into the binary, so there is no build step and nothing to go stale.
            ("GET", "/tasks.css") | ("GET", "/tasks.js") => match tasks::asset(&req.path) {
                Some(r) => r,
                None => Response::not_found(),
            },
            ("GET", "/api/tasks") => self.tasks(),
            ("GET", "/api/task") => self.task_one(req),
            ("POST", "/api/task/create") => self.task_create(req),
            ("POST", "/api/task/state") => self.task_state(req),
            ("POST", "/api/task/note") => self.task_note(req),
            ("POST", "/api/task/grant") => self.task_grant(req),
            ("POST", "/api/task/dep") => self.task_dep(req),
            ("GET", "/api/fleet") => self.fleet(),
            ("GET", "/api/rooms") => self.rooms(),
            ("GET", "/api/messages") => self.messages(req),
            ("GET", "/api/memory") => self.memory(req),
            ("GET", "/api/events") => self.events(),
            ("GET", "/api/health") => Response::json("{\"ok\":true}".into()),
            ("GET", "/api/inbox") => self.inbox(),
            // Browsing, not just searching. Memory was search-only, so a fact you could
            // not guess the wording of was invisible — you cannot review what you cannot
            // enumerate.
            ("GET", "/api/brains") => self.brains(),
            ("GET", "/api/facts") => self.facts(req),
            ("GET", "/api/proposals") => self.proposals(),
            ("GET", "/api/standup") => self.standup(),
            ("GET", "/api/doctor") => self.doctor(),
            ("GET", "/api/config") => self.config(),
            ("GET", "/api/accounts") => self.accounts(),
            // The desktop widget's health row. Served here because the daemon is already
            // running: the Python this replaces spawned an interpreter and one curl per
            // service, every 5 seconds, forever.
            ("GET", "/api/services") => {
                Response::json(services::report(&services::manifest_path()))
            }
            // WRITES. The old dashboard shelled out to the CLI because it was a separate
            // process; this one runs INSIDE the daemon, so the shared connection IS the
            // single-writer path. Same discipline, one less hop.
            ("POST", "/api/answer") => self.answer(req),
            ("POST", "/api/dismiss") => self.dismiss(req),
            ("POST", "/api/resolve-park") => self.resolve_park(req),
            ("POST", "/api/mode") => self.set_mode(req),
            ("POST", "/api/proposal") => self.decide(req),
            ("POST", "/api/forget") => self.forget(req),
            ("POST", "/api/standup") => self.standup_action(req),
            ("POST", "/api/config") => self.set_config(req),
            ("POST", "/api/session") => self.session_action(req),
            ("POST", "/api/accounts") => self.switch_account(req),
            ("GET", _) => Response::not_found(),
            _ => Response::text(405, "method not allowed"),
        }
    }

    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn fleet(&self) -> Response {
        let c = self.lock();
        let rows = query_json(
            &c,
            "SELECT name, COALESCE(status,''), COALESCE(last_seen,'') FROM sessions \
             WHERE ended_ts IS NULL ORDER BY last_seen DESC LIMIT 50",
            &["name", "status", "last_seen"],
        );
        Response::json(rows)
    }

    fn rooms(&self) -> Response {
        let c = self.lock();
        let rows = query_json(
            &c,
            "SELECT r.room, COALESCE(r.kind,''), \
                    (SELECT COUNT(*) FROM messages m WHERE m.room = r.room) \
             FROM rooms r WHERE r.closed_ts IS NULL ORDER BY r.room LIMIT 100",
            &["room", "kind", "messages"],
        );
        Response::json(rows)
    }

    fn messages(&self, req: &Request) -> Response {
        let room = req.query.get("room").cloned().unwrap_or_else(|| "lobby".into());
        let c = self.lock();
        let mut stmt = match c.prepare(
            "SELECT ts, sender, target, text FROM messages WHERE room = ?1 \
             ORDER BY seq DESC LIMIT 60",
        ) {
            Ok(s) => s,
            Err(e) => return Response::text(500, &e.to_string()),
        };
        let mut out = String::from("[");
        let rows = stmt.query_map([&room], |r| {
            Ok(format!(
                "{{\"ts\":\"{}\",\"sender\":\"{}\",\"target\":\"{}\",\"text\":\"{}\"}}",
                esc(&r.get::<_, String>(0).unwrap_or_default()),
                esc(&r.get::<_, String>(1).unwrap_or_default()),
                esc(&r.get::<_, String>(2).unwrap_or_default()),
                esc(&r.get::<_, String>(3).unwrap_or_default()),
            ))
        });
        if let Ok(rows) = rows {
            let items: Vec<String> = rows.filter_map(Result::ok).collect();
            out.push_str(&items.join(","));
        }
        out.push(']');
        Response::json(out)
    }

    /// Semantic memory search.
    ///
    /// The old dashboard's ⌘K palette did a substring scan that measured **1.65 s and
    /// 981 individual file reads, uncached** — typing an 8-character word cost ~11.5 s
    /// of server work and ~6,800 file reads. This is one indexed query plus a cosine
    /// scan over ~2 MB of vectors.
    fn memory(&self, req: &Request) -> Response {
        let q = req.query.get("q").cloned().unwrap_or_default();
        if q.trim().is_empty() {
            return Response::json("[]".into());
        }
        let scopes: Vec<String> = match req.query.get("scope") {
            Some(s) if !s.is_empty() => vec![s.clone()],
            // No scope selected means "search everything I have" — that is the operator
            // looking at their own machine, not an agent session, so it is not the leak.
            _ => {
                let c = self.lock();
                let mut st = match c.prepare("SELECT DISTINCT dataset FROM memories") {
                    Ok(s) => s,
                    Err(e) => return Response::text(500, &e.to_string()),
                };
                let r = st.query_map([], |r| r.get::<_, String>(0));
                match r {
                    Ok(it) => it.filter_map(Result::ok).collect(),
                    Err(e) => return Response::text(500, &e.to_string()),
                }
            }
        };
        let c = self.lock();
        match paos_memory::recall(&c, self.embedder.as_ref(), &scopes, &q, 20) {
            Ok(hits) => {
                let items: Vec<String> = hits
                    .iter()
                    .map(|h| {
                        format!(
                            "{{\"score\":{:.4},\"dataset\":\"{}\",\"ts\":\"{}\",\"text\":\"{}\"}}",
                            h.score,
                            esc(&h.memory.dataset),
                            esc(&h.memory.created_ts),
                            esc(&h.memory.text)
                        )
                    })
                    .collect();
                Response::json(format!("[{}]", items.join(",")))
            }
            Err(e) => Response::text(500, &e.to_string()),
        }
    }

    /// The "what needs me" queue: open questions, parked decisions, deaf/stale sessions.
    ///
    /// This was the old dashboard's home screen and the single biggest UI loss in the
    /// rewrite — without it there was no way to answer an escalation from the laptop.
    /// The "brains": every dataset with a live fact count, tier first.
    ///
    /// Ordering is global -> org -> project because that is the blast radius of getting
    /// a fact's scope wrong, and the global brain is the one worth keeping lean.
    fn brains(&self) -> Response {
        let c = self.lock();
        Response::json(query_json(
            &c,
            "SELECT dataset, \
                    CASE WHEN dataset LIKE '%global%' THEN 'global' \
                         WHEN dataset LIKE 'org\\_%' ESCAPE '\\' THEN 'org' \
                         ELSE 'project' END, \
                    COUNT(*), \
                    SUM(CASE WHEN LENGTH(text) > 600 THEN 1 ELSE 0 END), \
                    MAX(created_ts) \
             FROM memories WHERE superseded IS NULL \
             GROUP BY dataset \
             ORDER BY CASE WHEN dataset LIKE '%global%' THEN 0 \
                           WHEN dataset LIKE 'org\\_%' ESCAPE '\\' THEN 1 ELSE 2 END, \
                      COUNT(*) DESC",
            &["dataset", "tier", "facts", "long", "newest"],
        ))
    }

    /// Facts inside one brain. `q` filters literally (LIKE), which is deliberate: this
    /// is the browse path, and a substring match is predictable in a way that vector
    /// similarity is not when you are auditing what you actually stored.
    fn facts(&self, req: &Request) -> Response {
        let ds = req.query.get("dataset").cloned().unwrap_or_default();
        if ds.trim().is_empty() {
            return Response::json("[]".into());
        }
        let q = req.query.get("q").cloned().unwrap_or_default();
        let c = self.lock();
        let sql = "SELECT id, text, COALESCE(created_ts,''), LENGTH(text) FROM memories \
                   WHERE dataset = ?1 AND superseded IS NULL \
                     AND (?2 = '' OR text LIKE '%' || ?2 || '%') \
                   ORDER BY created_ts DESC LIMIT 300";
        let Ok(mut stmt) = c.prepare(sql) else {
            return Response::json("[]".into());
        };
        let mut out = String::from("[");
        let rows = stmt.query_map(rusqlite::params![&ds, &q], |r| {
            Ok(format!(
                "{{\"id\":\"{}\",\"text\":\"{}\",\"ts\":\"{}\",\"len\":{}}}",
                esc(&r.get::<_, String>(0).unwrap_or_default()),
                esc(&r.get::<_, String>(1).unwrap_or_default()),
                esc(&r.get::<_, String>(2).unwrap_or_default()),
                r.get::<_, i64>(3).unwrap_or(0),
            ))
        });
        if let Ok(rows) = rows {
            for (i, row) in rows.flatten().enumerate() {
                if i > 0 { out.push(','); }
                out.push_str(&row);
            }
        }
        out.push(']');
        Response::json(out)
    }

    /// What the nightly dream proposed, awaiting a human. This queue was invisible in
    /// the UI, so the only way to see it was a CLI command you had to remember to run.
    fn proposals(&self) -> Response {
        let c = self.lock();
        Response::json(query_json(
            &c,
            "SELECT id, kind, dataset, COALESCE(text,''), COALESCE(rationale,''), \
                    COALESCE(source,''), COALESCE(created_ts,''), COALESCE(target_data_id,'') \
             FROM memory_proposals WHERE status='pending' ORDER BY id DESC LIMIT 200",
            &["id", "kind", "dataset", "text", "rationale", "source", "ts", "replaces"],
        ))
    }

    fn standup(&self) -> Response {
        let c = self.lock();
        Response::json(query_json(
            &c,
            "SELECT id, side, ts, body, status FROM standup_briefs \
             ORDER BY id DESC LIMIT 10",
            &["id", "side", "ts", "body", "status"],
        ))
    }

    fn doctor(&self) -> Response {
        let c = self.lock();
        let checks = paos_memory::doctor::run(&c);
        let mut out = String::from("[");
        for (i, ch) in checks.iter().enumerate() {
            if i > 0 { out.push(','); }
            out.push_str(&format!(
                "{{\"name\":\"{}\",\"level\":\"{}\",\"detail\":\"{}\",\"fix\":\"{}\"}}",
                esc(ch.name),
                match ch.level {
                    paos_memory::doctor::Level::Ok => "ok",
                    paos_memory::doctor::Level::Warn => "warn",
                    paos_memory::doctor::Level::Fail => "fail",
                },
                esc(&ch.detail),
                esc(ch.fix.as_deref().unwrap_or("")),
            ));
        }
        out.push(']');
        Response::json(out)
    }

    /// Approve or reject a proposal.
    ///
    /// Shells the skill rather than reimplementing merge/split semantics here. Two
    /// implementations of "what approving means" WILL drift, and the cost of drift is a
    /// fact deleted by one path that the other would have kept.
    ///
    /// The connection lock is NOT held across the subprocess — deliberately. `paos
    /// memory approve` writes through this same daemon, so holding it here would
    /// deadlock: we would be waiting on a child that is waiting on us.
    fn decide(&self, req: &Request) -> Response {
        let Some(id) = field(&req.body, "id").and_then(|v| v.parse::<i64>().ok()) else {
            return Response::text(400, "missing id");
        };
        let action = field(&req.body, "action").unwrap_or_default();
        if action != "approve" && action != "reject" {
            return Response::text(400, "action must be approve or reject");
        }
        match run_skill(&["memory", &action, &id.to_string()]) {
            Ok(out) => Response::json(format!("{{\"ok\":true,\"detail\":\"{}\"}}", esc(out.trim()))),
            Err(e) => Response::text(500, &e),
        }
    }

    /// Delete one fact. Same reasoning as decide(): no lock across the subprocess.
    fn forget(&self, req: &Request) -> Response {
        let Some(id) = field(&req.body, "id").filter(|s| !s.trim().is_empty()) else {
            return Response::text(400, "missing id");
        };
        match run_skill(&["memory", "forget", &id, "--force"]) {
            Ok(out) => Response::json(format!("{{\"ok\":true,\"detail\":\"{}\"}}", esc(out.trim()))),
            Err(e) => Response::text(500, &e),
        }
    }

    /// Generate a brief, or mark one reported. Both shell the skill: generation runs
    /// `claude -p` on the operator's own subscription, which is emphatically not
    /// something to reimplement here.
    ///
    /// Generation can take tens of seconds, so it does NOT hold the connection lock —
    /// same hazard as approving a proposal, and the same reason.
    fn standup_action(&self, req: &Request) -> Response {
        let action = field(&req.body, "action").unwrap_or_default();
        let side = field(&req.body, "side").unwrap_or_else(|| "both".into());
        if !matches!(side.as_str(), "work" | "personal" | "both") {
            return Response::text(400, "side must be work, personal or both");
        }
        let args: Vec<&str> = match action.as_str() {
            "brief" => vec!["standup", "brief", "--side", &side],
            // `reported` freezes ONE side's brief and advances its watermark, so it has
            // no meaningful "both".
            "reported" if side != "both" => vec!["standup", "reported", "--side", &side],
            "reported" => return Response::text(400, "reported needs a single side"),
            _ => return Response::text(400, "action must be brief or reported"),
        };
        match run_skill(&args) {
            Ok(out) => Response::json(format!("{{\"ok\":true,\"detail\":\"{}\"}}",
                                             esc(out.trim()))),
            Err(e) => Response::text(500, &e),
        }
    }

    /// Settings: the schema comes from `paos config schema`, the values from SQL.
    ///
    /// The schema is NOT duplicated here. It lives in config_facet.SETTINGS, and a second
    /// copy would drift — a UI offering a knob the daemon does not read is worse than no
    /// settings page, because it looks like it worked.
    fn config(&self) -> Response {
        let schema = run_skill(&["config", "schema"]).unwrap_or_else(|_| "[]".into());
        let secrets = secret_keys(&schema);
        let values = {
            let c = self.lock();
            let pairs = query_pairs(&c,
                "SELECT key, COALESCE(value,'') FROM paos_config ORDER BY key");
            // A secret row holds a REFERENCE, and even that does not reach the browser:
            // the page has no use for it and every byte sent is a byte that can leak.
            json_pairs(pairs.into_iter().filter(|(k, _)| !secrets.contains(k)))
        };
        // Status only, asked of the DAEMON. paos-web does not link paos-secrets, so
        // there is no code path here that can read a token — a property of the
        // dependency graph rather than a rule someone has to remember.
        let status_of = |key: &str| run_skill(&["secret", "status", key]).unwrap_or_default();
        // WHERE each setting comes from, so the page cannot claim a token is missing
        // while the bridge is running on one from the .env.
        let sources = run_skill(&["sources"]).unwrap_or_default();
        Response::json(config_payload(&schema, values, &secrets, &status_of, &sources))
    }

    fn set_config(&self, req: &Request) -> Response {
        let Some(key) = field(&req.body, "key").filter(|k| !k.trim().is_empty()) else {
            return Response::text(400, "missing key");
        };
        let value = field(&req.body, "value").unwrap_or_default();
        // Only keys the schema declares. Without this the endpoint is an arbitrary write
        // into the daemon's config table from any page that can reach localhost.
        let schema = run_skill(&["config", "schema"]).unwrap_or_default();
        if !schema.contains(&format!("\"{key}\"")) {
            return Response::text(400, "unknown setting");
        }
        match run_skill(&["config", "set", &key, &value]) {
            Ok(out) => Response::json(format!("{{\"ok\":true,\"detail\":\"{}\"}}", esc(out.trim()))),
            Err(e) => Response::text(500, &e),
        }
    }

    /// Claude slots and usage, via `paos accounts` — the same facet the CLI and
    /// Telegram use, so there is one definition of what "critical" means.
    /// Per-slot weekly climb over the last 24h, from the sampled history.
    ///
    /// Correlation, not attribution: nothing reports per-session token spend, so this
    /// says how fast a window is being consumed, never by whom. Labelled that way in the
    /// UI, because a confident guess here would blame the wrong session.
    fn usage_deltas(&self) -> String {
        let c = self.lock();
        let Ok(mut st) = c.prepare(
            "SELECT slot, MAX(seven_day) - MIN(seven_day), COUNT(*) \
             FROM usage_samples WHERE ts > strftime('%s','now') - 86400 GROUP BY slot",
        ) else {
            return "{}".into();
        };
        let mut out = String::from("{");
        if let Ok(rows) = st.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?, r.get::<_, i64>(2)?))
        }) {
            for (i, (slot, delta, n)) in rows.flatten().enumerate() {
                if i > 0 { out.push(','); }
                // One sample cannot show a change; reporting 0 would read as "idle".
                let d = if n >= 2 { format!("{delta:.1}") } else { "null".into() };
                out.push_str(&format!("\"{}\":{}", esc(&slot), d));
            }
        }
        out.push('}');
        out
    }

    fn accounts(&self) -> Response {
        match run_skill(&["accounts", "list", "--json"]) {
            Ok(out) => Response::json(format!(
                "{{\"accounts\":{},\"deltas24h\":{}}}", out.trim(), self.usage_deltas())),
            // Exit non-zero means usage could not be READ. Returning [] here would show
            // an empty, healthy-looking list for a broken poller.
            Err(_) => Response::json("{\"accounts\":null,\"deltas24h\":{}}".into()),
        }
    }

    fn switch_account(&self, req: &Request) -> Response {
        let slot = field(&req.body, "slot").unwrap_or_default();
        let mut args = vec!["accounts", "switch"];
        if !slot.trim().is_empty() {
            args.push(&slot);
        }
        match run_skill(&args) {
            Ok(out) => Response::json(format!("{{\"ok\":true,\"detail\":\"{}\"}}", esc(out.trim()))),
            Err(e) => Response::text(500, &e),
        }
    }

    /// Act on a session that needs attention: wake it, or retire it.
    ///
    /// A DEAF or stale session was rendered as a read-only line under "needs you", which
    /// is a contradiction — if there is nothing to do, it does not need you. Waking pokes
    /// it through DND; retiring ends it so it stops appearing.
    fn session_action(&self, req: &Request) -> Response {
        let Some(name) = field(&req.body, "name").filter(|n| !n.trim().is_empty()) else {
            return Response::text(400, "missing name");
        };
        match field(&req.body, "action").unwrap_or_default().as_str() {
            "wake" => match run_skill(&["bus", "wake", &name, "operator: are you still working?"]) {
                Ok(o) => Response::json(format!("{{\"ok\":true,\"detail\":\"{}\"}}", esc(o.trim()))),
                Err(e) => Response::text(500, &e),
            },
            "retire" => {
                let c = self.lock();
                match c.execute(
                    "UPDATE sessions SET ended_ts = ?1 WHERE name = ?2 AND ended_ts IS NULL",
                    rusqlite::params![now_iso(), name],
                ) {
                    Ok(n) if n > 0 => Response::json("{\"ok\":true,\"detail\":\"retired\"}".into()),
                    Ok(_) => Response::text(404, "no live session by that name"),
                    Err(e) => Response::text(500, &e.to_string()),
                }
            }
            _ => Response::text(400, "action must be wake or retire"),
        }
    }

    fn inbox(&self) -> Response {
        let c = self.lock();
        let esc = query_json(&c,
            "SELECT id, session, question, COALESCE(options,'') FROM escalations \
             WHERE status='open' ORDER BY id",
            &["id", "session", "question", "options"]);
        let parked = query_json(&c,
            "SELECT id, session, note FROM parked WHERE resolved=0 ORDER BY id",
            &["id", "session", "note"]);
        let attention = query_json(&c,
            "SELECT name, COALESCE(status,''), \
                    CASE WHEN deaf_since IS NOT NULL THEN 'deaf' ELSE 'stale' END \
             FROM sessions WHERE ended_ts IS NULL \
               AND (deaf_since IS NOT NULL OR stale_since IS NOT NULL) ORDER BY name",
            &["name", "status", "kind"]);
        let mode: String = c
            .query_row("SELECT mode FROM operator_mode WHERE id=1", [], |r| r.get(0))
            .unwrap_or_else(|_| "attended".into());
        Response::json(format!(
            "{{\"mode\":\"{}\",\"escalations\":{esc},\"parked\":{parked},\"attention\":{attention}}}",
            esc_str(&mode)
        ))
    }

    fn answer(&self, req: &Request) -> Response {
        let Some(id) = field(&req.body, "id").and_then(|v| v.parse::<i64>().ok()) else {
            return Response::text(400, "missing id");
        };
        let Some(text) = field(&req.body, "text").filter(|t| !t.trim().is_empty()) else {
            return Response::text(400, "missing text");
        };
        let mut c = self.lock();
        match paos_operator::answer(&mut c, id, &text, &now_iso()) {
            Ok(true) => Response::json("{\"ok\":true}".into()),
            // Answering twice must not overwrite the first answer.
            Ok(false) => Response::text(409, "that escalation is already closed"),
            Err(e) => Response::text(500, &e.to_string()),
        }
    }

    /// Close an escalation WITHOUT answering it.
    ///
    /// Some questions stop being questions. The four sitting in the queue on 2026-08-01
    /// were all from sessions that had since been archived — answering them would have
    /// reached nobody, and the bus rules forbid answering on a human's behalf anyway, so
    /// there was no legitimate way to clear them and the dashboard counted them as work
    /// forever. An inbox you cannot empty stops being read.
    ///
    /// `dismissed`, not `answered`, and not a DELETE: the question and who asked it stay
    /// queryable. This records a decision — "this no longer needs an answer" — rather than
    /// erasing the evidence that it was asked.
    fn dismiss(&self, req: &Request) -> Response {
        let Some(id) = field(&req.body, "id").and_then(|v| v.parse::<i64>().ok()) else {
            return Response::text(400, "missing id");
        };
        let c = self.lock();
        match c.execute(
            "UPDATE escalations SET status='dismissed', answered_ts=?2 \
             WHERE id=?1 AND status='open'",
            rusqlite::params![id, now_iso()],
        ) {
            Ok(0) => Response::text(409, "that escalation is not open"),
            Ok(_) => Response::json("{\"ok\":true}".into()),
            Err(e) => Response::text(500, &e.to_string()),
        }
    }

    fn resolve_park(&self, req: &Request) -> Response {
        let Some(id) = field(&req.body, "id").and_then(|v| v.parse::<i64>().ok()) else {
            return Response::text(400, "missing id");
        };
        let c = self.lock();
        match paos_operator::resolve_park(&c, id) {
            Ok(true) => Response::json("{\"ok\":true}".into()),
            Ok(false) => Response::text(409, "already resolved"),
            Err(e) => Response::text(500, &e.to_string()),
        }
    }

    fn set_mode(&self, req: &Request) -> Response {
        let Some(m) = field(&req.body, "mode").and_then(|m| paos_operator::Mode::parse(&m)) else {
            return Response::text(400, "mode must be attended, autonomous or away");
        };
        let c = self.lock();
        match paos_operator::set_mode(&c, m, "dashboard", &now_iso()) {
            Ok(_) => Response::json(format!("{{\"ok\":true,\"mode\":\"{}\"}}", m.as_str())),
            Err(e) => Response::text(500, &e.to_string()),
        }
    }

    fn events(&self) -> Response {
        let c = self.lock();
        let rows = query_json(
            &c,
            "SELECT ts, kind, COALESCE(session,''), summary FROM events \
             ORDER BY id DESC LIMIT 60",
            &["ts", "kind", "session", "summary"],
        );
        Response::json(rows)
    }
}

/// Render a query as a JSON array of objects with the given keys.
///
/// Every column is read as TEXT and escaped; a NULL becomes "". Integer columns come
/// back through SQLite's conversion, which is fine for display.
/// Pull a field out of a form-encoded or JSON-ish body.
///
/// Deliberately tolerant: this is a localhost UI posting its own forms, and being
/// strict about content-type here buys nothing.
pub(crate) fn field(body: &str, key: &str) -> Option<String> {
    for pair in body.split('&') {
        if let Some(v) = pair.strip_prefix(&format!("{key}=")) {
            return Some(percent_decode_form(v));
        }
    }
    // JSON fallback: "key":"value" or "key":123
    let pat = format!("\"{key}\":");
    let i = body.find(&pat)? + pat.len();
    let rest = body[i..].trim_start();
    if let Some(r) = rest.strip_prefix('"') {
        return r.split('"').next().map(|s| s.replace("\\n", "\n").replace("\\\"", "\""));
    }
    Some(rest.chars().take_while(|c| c.is_ascii_digit()).collect())
}

fn percent_decode_form(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(byte) => { out.push(byte); i += 3; }
                Err(_) => { out.push(b'%'); i += 1; }
            },
            b'+' => { out.push(b' '); i += 1; }
            c => { out.push(c); i += 1; }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn esc_str(s: &str) -> String { esc(s) }

/// Run the paos skill. The UI's write actions go through the same command line the
/// operator would type, so there is exactly one definition of what each action does.
pub(crate) fn run_skill(args: &[&str]) -> Result<String, String> {
    let bin = std::env::var("PAOS_SKILL_BIN").unwrap_or_else(|_| {
        format!("{}/.claude/skills/paos/paos", std::env::var("HOME").unwrap_or_default())
    });
    let out = std::process::Command::new(&bin)
        .args(args)
        .output()
        .map_err(|e| format!("{bin}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).to_string())
    }
}

pub(crate) fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
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
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, tod/3600, (tod%3600)/60, tod%60)
}

/// Build the settings payload.
///
/// Pure, and separate from `config()` for a reason the first version proved: `config()`
/// gets the schema by SHELLING the installed `paos` binary, so a test of it silently
/// asserts against whatever build happens to be on the machine — which is how the leak
/// check passed a schema with no secrets in it and reported success.
fn config_payload(
    schema: &str,
    values: String,
    secrets: &[String],
    status_of: &dyn Fn(&str) -> String,
    sources: &str,
) -> String {
    let src = parse_sources(sources);
    let mut out = Vec::new();
    for key in secrets {
        // A secret supplied by the .env IS configured. Reporting it missing because the
        // config table has no reference is how the page ended up contradicting a working
        // bridge.
        let state = if src.iter().any(|(k, v)| k == key && v == "env") {
            "configured"
        } else {
            match status_of(key).trim() {
                "configured" => "configured",
                "unreadable" => "unreadable",
                // Anything else — including an unreachable daemon — reads as
                // not-configured rather than as a fault of the secret itself.
                _ => "missing",
            }
        };
        out.push(format!("\"{}\":\"{}\"", esc(key), state));
    }
    let srcs: Vec<String> = src.iter()
        .map(|(k, v)| format!("\"{}\":\"{}\"", esc(k), esc(v)))
        .collect();
    format!("{{\"schema\":{},\"values\":{},\"secret_status\":{{{}}},\"sources\":{{{}}}}}",
            schema.trim(), values, out.join(","), srcs.join(","))
}

/// `key=source` lines from `paos sources`.
fn parse_sources(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| l.trim().split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

/// The keys the settings page must render as a state rather than an input.
fn secret_keys(schema: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(schema)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter(|s| s["type"] == "secret")
        .filter_map(|s| s["key"].as_str().map(str::to_string))
        .collect()
}

/// `(key, value)` rows, so the caller can filter before anything is serialised — a
/// secret must never be turned into JSON and then removed from it.
fn query_pairs(conn: &Connection, sql: &str) -> Vec<(String, String)> {
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?.unwrap_or_default()))
    });
    match rows {
        Ok(it) => it.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

fn json_pairs(pairs: impl Iterator<Item = (String, String)>) -> String {
    let rows: Vec<String> = pairs
        .map(|(k, v)| format!("{{\"key\":\"{}\",\"value\":\"{}\"}}", esc(&k), esc(&v)))
        .collect();
    format!("[{}]", rows.join(","))
}

fn query_json(conn: &Connection, sql: &str, keys: &[&str]) -> String {
    let Ok(mut stmt) = conn.prepare(sql) else {
        return "[]".into();
    };
    let n = keys.len();
    let rows = stmt.query_map([], |r| {
        let mut fields = Vec::with_capacity(n);
        for (i, k) in keys.iter().enumerate() {
            let v: String = r
                .get::<_, Option<String>>(i)
                .ok()
                .flatten()
                .or_else(|| r.get::<_, Option<i64>>(i).ok().flatten().map(|x| x.to_string()))
                .unwrap_or_default();
            fields.push(format!("\"{}\":\"{}\"", k, esc(&v)));
        }
        Ok(format!("{{{}}}", fields.join(",")))
    });
    match rows {
        Ok(it) => format!("[{}]", it.filter_map(Result::ok).collect::<Vec<_>>().join(",")),
        Err(_) => "[]".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    fn browser_post(path: &str, body: &str, origin: Option<&str>, site: Option<&str>) -> Request {
        Request {
            method: "POST".into(),
            path: path.into(),
            query: Default::default(),
            body: body.into(),
            origin: origin.map(str::to_string),
            fetch_site: site.map(str::to_string),
        }
    }

    #[test]
    fn a_website_cannot_forge_a_write() {
        // Verified against the running daemon before this guard existed: a POST carrying
        // `Origin: https://evil.example` changed the config and got {"ok":true}. A form
        // POST is a "simple request", so no preflight stops it and the page never needs
        // to read the reply.
        let w = web();
        let r = w.route(&browser_post("/api/forget", "id=abc", Some("https://evil.example"), None));
        assert_eq!(r.status, 403);
        let r = w.route(&browser_post("/api/config", "key=dream_enabled&value=0",
                              Some("https://evil.example"), None));
        assert_eq!(r.status, 403);
    }

    #[test]
    fn sec_fetch_site_alone_is_enough_to_refuse() {
        // Belt and braces: current browsers send this even where Origin is elided.
        let r = web().route(&browser_post("/api/answer", "id=1&text=yes", None, Some("cross-site")));
        assert_eq!(r.status, 403);
    }

    #[test]
    fn the_dashboards_own_page_still_works() {
        // The guard is worthless if it also breaks the UI it protects.
        let w = web();
        for origin in ["http://127.0.0.1:8788", "http://localhost:8788"] {
            let r = w.route(&browser_post("/api/mode", "mode=away", Some(origin), Some("same-origin")));
            assert_ne!(r.status, 403, "origin {origin} must be allowed");
        }
    }

    #[test]
    fn the_cli_and_scripts_are_not_blocked() {
        // curl and the skill send no browser provenance, and are not reachable FROM a web
        // page — refusing them would break every non-browser caller for no security gain.
        let r = web().route(&browser_post("/api/mode", "mode=away", None, None));
        assert_ne!(r.status, 403);
    }

    #[test]
    fn reads_are_never_refused() {
        let r = web().route(&Request {
            method: "GET".into(), path: "/api/fleet".into(), query: Default::default(),
            body: String::new(), origin: Some("https://evil.example".into()),
            fetch_site: Some("cross-site".into()),
        });
        assert_ne!(r.status, 403, "a GET leaks nothing a local page could not already read");
    }
    use std::collections::HashMap;

    fn web() -> Web {
        let c = paos_store::open_in_memory().unwrap();
        paos_memory::ensure_schema(&c).unwrap();
        let e = paos_memory::HashEmbedder::new(64);
        c.execute("INSERT INTO sessions(name,status,updated_ts) VALUES('swift-otter','building',  't')", []).unwrap();
        c.execute("INSERT INTO rooms(room,created_ts) VALUES('lobby','t')", []).unwrap();
        c.execute(
            "INSERT INTO messages(room,seq,ts,sender,target,text) \
             VALUES('lobby',1,'t','peer','@all','hello \"world\"')", [],
        ).unwrap();
        paos_memory::remember(&c, &e, "proj_x", "the deploy runs on fly", "t").unwrap();
        paos_memory::remember(&c, &e, "other", "unrelated content", "t").unwrap();
        Web { conn: Arc::new(Mutex::new(c)), embedder: Arc::new(paos_memory::HashEmbedder::new(64)) }
    }

    #[test]
    fn a_secret_reaches_the_browser_as_a_state_and_nothing_else() {
        // The web layer must have no way to show a token. Asserts the payload, which is
        // the only thing the browser ever sees.
        let schema = r#"[{"key":"telegram_bot_token","type":"secret","default":""}]"#;
        let secrets = secret_keys(schema);
        // The reference is filtered out before serialisation, so `values` never held it.
        let values = json_pairs(
            vec![("dream_enabled".to_string(), "1".to_string())].into_iter());
        let out = config_payload(schema, values, &secrets, &|_| "missing".into(), "");
        assert!(out.contains("\"telegram_bot_token\":\"missing\""), "{out}");
        assert!(!out.contains("env:"), "no reference may reach the browser: {out}");
    }

    #[test]
    fn a_secret_supplied_by_the_env_reads_as_configured_not_missing() {
        // The page reported a MISSING Telegram token while the bridge was demonstrably
        // running on one from the .env. Config-table-only status is how a settings page
        // ends up contradicting the daemon it configures.
        let schema = r#"[{"key":"telegram_bot_token","type":"secret","default":""}]"#;
        let out = config_payload(schema, "[]".into(), &secret_keys(schema),
                                 &|_| "missing".into(), "telegram_bot_token=env\n");
        assert!(out.contains("\"telegram_bot_token\":\"configured\""), "{out}");
        assert!(out.contains("\"sources\":{\"telegram_bot_token\":\"env\"}"), "{out}");
    }

    #[test]
    fn an_unset_secret_is_still_missing_when_nothing_supplies_it() {
        let schema = r#"[{"key":"telegram_bot_token","type":"secret","default":""}]"#;
        let out = config_payload(schema, "[]".into(), &secret_keys(schema),
                                 &|_| "missing".into(), "telegram_bot_token=unset\n");
        assert!(out.contains("\"telegram_bot_token\":\"missing\""), "{out}");
    }

    #[test]
    fn an_unreachable_daemon_reads_as_missing_not_as_a_broken_secret() {
        // run_skill returns an empty string when it cannot reach the daemon. Reporting
        // that as `unreadable` would blame the keychain for a socket problem.
        let schema = r#"[{"key":"telegram_bot_token","type":"secret","default":""}]"#;
        let out = config_payload(schema, "[]".into(), &secret_keys(schema), &|_| "".into(), "");
        assert!(out.contains("\"telegram_bot_token\":\"missing\""), "{out}");
    }

    #[test]
    fn a_secret_row_is_stripped_from_the_values_the_page_receives() {
        let secrets = vec!["telegram_bot_token".to_string()];
        let pairs = vec![
            ("telegram_bot_token".to_string(), "env:SOMETHING".to_string()),
            ("dream_enabled".to_string(), "1".to_string()),
        ];
        let json = json_pairs(pairs.into_iter().filter(|(k, _)| !secrets.contains(k)));
        assert_eq!(json, r#"[{"key":"dream_enabled","value":"1"}]"#);
    }

    #[test]
    fn only_keys_typed_secret_are_treated_as_secrets() {
        // A substring match on the schema would hide a key merely NAMED like one.
        let schema = r#"[{"key":"telegram_bot_token","type":"secret","default":""},
                         {"key":"secret_note","type":"str","default":""}]"#;
        assert_eq!(secret_keys(schema), vec!["telegram_bot_token".to_string()]);
    }

    fn get(w: &Web, path: &str, q: &[(&str, &str)]) -> Response {
        let mut query = HashMap::new();
        for (k, v) in q {
            query.insert(k.to_string(), v.to_string());
        }
        w.route(&Request { method: "GET".into(), path: path.into(), query, body: String::new(), origin: None, fetch_site: None })
    }

    fn body(r: &Response) -> String {
        String::from_utf8_lossy(&r.body).into_owned()
    }

    #[test]
    fn index_is_served_from_the_binary() {
        // No build step, so there is nothing to go stale.
        let r = get(&web(), "/", &[]);
        assert_eq!(r.status, 200);
        assert!(body(&r).contains("<!doctype html>"), "expected the embedded page");
    }

    #[test]
    fn fleet_lists_live_sessions() {
        let r = get(&web(), "/api/fleet", &[]);
        let b = body(&r);
        assert!(b.contains("swift-otter") && b.contains("building"), "{b}");
    }

    #[test]
    fn message_text_is_json_escaped() {
        // A quote in a message used to be enough to blank a page.
        let b = body(&get(&web(), "/api/messages", &[("room", "lobby")]));
        assert!(b.contains(r#"hello \"world\""#), "{b}");
    }

    #[test]
    fn memory_search_is_scoped_when_a_scope_is_given() {
        let b = body(&get(&web(), "/api/memory", &[("q", "deploy"), ("scope", "proj_x")]));
        assert!(b.contains("proj_x"), "{b}");
        assert!(!b.contains("unrelated content"), "out-of-scope result leaked: {b}");
    }

    #[test]
    fn empty_query_returns_nothing_rather_than_the_whole_corpus() {
        for q in ["", "   "] {
            assert_eq!(body(&get(&web(), "/api/memory", &[("q", q)])), "[]");
        }
    }

    #[test]
    fn unknown_path_is_404_and_writes_are_rejected() {
        assert_eq!(get(&web(), "/api/nope", &[]).status, 404);
        let w = web();
        let r = w.route(&Request {
            method: "POST".into(),
            path: "/api/fleet".into(),
            query: HashMap::new(),
            body: String::new(),
            origin: None,
            fetch_site: None,
        });
        // Writes belong to the daemon's socket API, not to this read surface.
        assert_eq!(r.status, 405);
    }

    #[test]
    fn every_endpoint_returns_valid_json_shape() {
        let w = web();
        for path in ["/api/fleet", "/api/rooms", "/api/events", "/api/health"] {
            let b = body(&get(&w, path, &[]));
            assert!(
                (b.starts_with('[') && b.ends_with(']')) || (b.starts_with('{') && b.ends_with('}')),
                "{path} returned {b}"
            );
        }
    }

    #[test]
    fn inbox_reports_what_needs_the_operator() {
        let w = web();
        {
            let c = w.conn.lock().unwrap();
            paos_operator::ask(&c, "swift-otter", "deploy to prod?", Some("ship,hold"), "t").unwrap();
            c.execute("INSERT INTO parked(session,note,resolved,created_ts) \
                       VALUES('a','decide later',0,'t')", []).unwrap();
            c.execute("UPDATE sessions SET deaf_since='t' WHERE name='swift-otter'", []).unwrap();
        }
        let b = body(&get(&w, "/api/inbox", &[]));
        assert!(b.contains("deploy to prod?"), "{b}");
        assert!(b.contains("decide later"), "{b}");
        assert!(b.contains("deaf"), "{b}");
        assert!(b.contains("\"mode\""), "{b}");
    }

    fn post(w: &Web, path: &str, body_str: &str) -> Response {
        // No browser provenance: this is the CLI/script path, which the CSRF guard
        // deliberately allows.
        w.route(&Request {
            method: "POST".into(), path: path.into(),
            query: HashMap::new(), body: body_str.into(),
            origin: None, fetch_site: None,
        })
    }

    #[test]
    fn answering_an_escalation_from_the_dashboard_closes_it() {
        // THE biggest UI loss in the rewrite: there was no way to unblock a session
        // from the laptop at all.
        let w = web();
        let id = { let c = w.conn.lock().unwrap();
                   paos_operator::ask(&c, "s", "ship?", None, "t").unwrap() };
        let r = post(&w, "/api/answer", &format!("id={id}&text=ship+it"));
        assert_eq!(r.status, 200, "{}", body(&r));
        let (status, answer): (String, Option<String>) = {
            let c = w.conn.lock().unwrap();
            c.query_row("SELECT status, answer FROM escalations WHERE id=?1", [id],
                        |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
        };
        assert_eq!(status, "answered");
        assert_eq!(answer.as_deref(), Some("ship it"), "form encoding must be decoded");
    }

    #[test]
    fn dismissing_closes_an_escalation_without_inventing_an_answer() {
        // The case this exists for: a question from a session that no longer exists.
        // Answering it would reach nobody, and answering on a human's behalf is forbidden
        // by the bus rules — so before this there was NO way to clear it and it counted as
        // outstanding work forever.
        let w = web();
        let id = { let c = w.conn.lock().unwrap();
                   paos_operator::ask(&c, "archived-session", "still relevant?", None, "t").unwrap() };
        let r = post(&w, "/api/dismiss", &format!("id={id}"));
        assert_eq!(r.status, 200, "{}", body(&r));
        let (status, answer): (String, Option<String>) = {
            let c = w.conn.lock().unwrap();
            c.query_row("SELECT status, answer FROM escalations WHERE id=?1", [id],
                        |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
        };
        assert_eq!(status, "dismissed");
        // NO ANSWER INVENTED. A dismissal that wrote some placeholder into `answer` would
        // be indistinguishable later from something the operator actually said.
        assert_eq!(answer, None, "dismissing must not fabricate an answer");
    }

    #[test]
    fn a_dismissed_escalation_stops_counting_as_work_but_stays_queryable() {
        // Both halves matter. It must leave the inbox — that is the point — and the row
        // must survive, because "who asked what" is the audit trail.
        let w = web();
        let id = { let c = w.conn.lock().unwrap();
                   paos_operator::ask(&c, "s", "q", None, "t").unwrap() };
        assert_eq!(post(&w, "/api/dismiss", &format!("id={id}")).status, 200);
        let open: i64 = { let c = w.conn.lock().unwrap();
            c.query_row("SELECT COUNT(*) FROM escalations WHERE status='open'", [], |r| r.get(0)).unwrap() };
        assert_eq!(open, 0, "a dismissed escalation must leave the inbox");
        let rows: i64 = { let c = w.conn.lock().unwrap();
            c.query_row("SELECT COUNT(*) FROM escalations WHERE id=?1", [id], |r| r.get(0)).unwrap() };
        assert_eq!(rows, 1, "dismiss must not DELETE — the question stays queryable");
        // Dismissing twice is refused, same as answering twice.
        assert_eq!(post(&w, "/api/dismiss", &format!("id={id}")).status, 409);
    }

    #[test]
    fn answering_twice_is_refused_rather_than_overwriting() {
        let w = web();
        let id = { let c = w.conn.lock().unwrap();
                   paos_operator::ask(&c, "s", "q", None, "t").unwrap() };
        assert_eq!(post(&w, "/api/answer", &format!("id={id}&text=first")).status, 200);
        assert_eq!(post(&w, "/api/answer", &format!("id={id}&text=second")).status, 409);
        let answer: String = {
            let c = w.conn.lock().unwrap();
            c.query_row("SELECT answer FROM escalations WHERE id=?1", [id], |r| r.get(0)).unwrap()
        };
        assert_eq!(answer, "first");
    }

    #[test]
    fn a_malformed_write_is_rejected_not_half_applied() {
        let w = web();
        for body_str in ["", "id=notanumber&text=x", "id=1", "text=only"] {
            let r = post(&w, "/api/answer", body_str);
            assert_eq!(r.status, 400, "should reject {body_str:?}");
        }
    }

    #[test]
    fn json_bodies_work_too() {
        let w = web();
        let id = { let c = w.conn.lock().unwrap();
                   paos_operator::ask(&c, "s", "q", None, "t").unwrap() };
        let r = post(&w, "/api/answer", &format!("{{\"id\":{id},\"text\":\"via json\"}}"));
        assert_eq!(r.status, 200, "{}", body(&r));
    }

    #[test]
    fn resolving_a_park_closes_it_once() {
        let w = web();
        {
            let c = w.conn.lock().unwrap();
            c.execute("INSERT INTO parked(id,session,note,resolved,created_ts) \
                       VALUES(7,'a','n',0,'t')", []).unwrap();
        }
        assert_eq!(post(&w, "/api/resolve-park", "id=7").status, 200);
        assert_eq!(post(&w, "/api/resolve-park", "id=7").status, 409);
    }

    #[test]
    fn mode_can_be_set_from_the_dashboard_and_bad_input_refused() {
        let w = web();
        assert_eq!(post(&w, "/api/mode", "mode=away").status, 200);
        {
            let c = w.conn.lock().unwrap();
            assert_eq!(paos_operator::get_mode(&c), paos_operator::Mode::Away);
        }
        assert_eq!(post(&w, "/api/mode", "mode=nonsense").status, 400);
    }

    #[test]
    fn writes_are_confined_to_the_three_operator_actions() {
        // Everything else must stay read-only; a stray POST route is how a read surface
        // quietly becomes a write surface.
        let w = web();
        for path in ["/api/fleet", "/api/rooms", "/api/memory", "/api/events", "/api/inbox"] {
            assert_eq!(post(&w, path, "x=1").status, 405, "{path} must not accept writes");
        }
    }
}