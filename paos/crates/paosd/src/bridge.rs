//! The Telegram bridge: the loop that keeps the operator reachable.
//!
//! Three jobs, all gated on `may_push` so the phone stays silent unless the operator
//! opened the channel:
//!   * **inbound**  — long-poll getUpdates; an operator message becomes a bus message
//!                    from the `operator` identity, which is the one sanctioned way a
//!                    human reaches a session.
//!   * **escalations** — unpushed asks go out, once.
//!   * **mirror**   — new bus traffic is copied to Telegram, silently, EXCEPT messages
//!                    addressed to the operator, which notify.
//!
//! Coexistence matters more than elegance here. Python sessions write the same SQLite
//! file and their listeners poll it, so they keep working untouched while this runs —
//! the mirror reads whatever landed, regardless of which implementation wrote it.

use paos_operator as op;
use paos_operator::telegram::{self, Config};
use rusqlite::Connection;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Bound one mirror pass. Telegram caps a group near 20 messages/minute, and a wall of
/// messages is unreadable on a phone anyway.
const MIRROR_BATCH: i64 = 15;
/// Sustained ceiling, messages/minute. Telegram caps a group near 20/min. The previous
/// version sent up to MIRROR_BATCH *per 2-second pass* — 450/min — which guarantees 429
/// storms during any fleet burst.
const MIRROR_RATE_PER_MIN: f64 = 15.0;
/// Attempts on one message before dropping it loudly. Without a cap, a single message
/// Telegram will never accept blocks ALL delivery forever.
const MAX_SEND_ATTEMPTS: u32 = 5;

#[derive(Default)]
struct Limiter {
    tokens: f64,
    last: Option<std::time::Instant>,
    stuck_id: i64,
    stuck_n: u32,
}

impl Limiter {
    fn allowance(&mut self) -> i64 {
        let now = std::time::Instant::now();
        match self.last {
            None => self.tokens = MIRROR_RATE_PER_MIN,
            Some(prev) => {
                let secs = now.duration_since(prev).as_secs_f64();
                self.tokens = (self.tokens + secs * MIRROR_RATE_PER_MIN / 60.0)
                    .min(MIRROR_RATE_PER_MIN);
            }
        }
        self.last = Some(now);
        (self.tokens as i64).min(MIRROR_BATCH)
    }
    fn spend(&mut self, n: i64) {
        self.tokens = (self.tokens - n as f64).max(0.0);
    }
}

/// How often the supervisor sweeps. Cheap (one query over live sessions) and this is
/// the only thing that makes a deaf session visible, so it runs regardless of mode.
const SUPERVISE_EVERY_SECS: u64 = 60;

pub fn spawn(
    conn: Arc<Mutex<Connection>>,
    cfg: Config,
    embedder: Arc<dyn paos_memory::Embedder>,
) {
    // ONE consumer per bot token, machine-wide.
    //
    // paosd's singleton lock is the unix socket, which is keyed on PAOS_ROOT — so a
    // daemon started with a different root (a test instance, a worktree build) passes
    // that check and then happily long-polls the SAME bot. Telegram hands each update to
    // whichever consumer asks first, so messages are lost at random and nothing errors.
    //
    // This is not hypothetical: two test daemons I started with PAOS_ROOT=$TMPDIR ran for
    // four hours stealing the operator's messages, and the only symptom was that he had
    // to say "you're not reading me" twice. The lock is on the TOKEN because that is what
    // is actually shared, not the database.
    // THE LOCK IS NOT ENOUGH, and 2026-08-03 proved it: it is first-come, so a binary
    // built in a scratch checkout can beat the real daemon to it — and then the REAL
    // daemon disables its own bridge and says so only on stderr nobody reads. The
    // operator spent hours with commands that intermittently did nothing.
    //
    // Note what does NOT save you: unsetting TELEGRAM_BOT_TOKEN. `main` reads
    // `$HOME/.claude/skills/paos/.env` by absolute path, so ANY paosd anywhere finds the
    // real token whether or not it is in the environment.
    //
    // So the installed binary is the only one allowed to bridge. `PAOS_ALLOW_BRIDGE=1`
    // exists for the person who genuinely means to run an uninstalled build against the
    // real bot; it has to be typed, which is the whole point.
    if !may_bridge(&std::env::current_exe().ok(), &std::env::var("PAOS_ALLOW_BRIDGE").ok(),
                   &installed_paosd()) {
        eprintln!("paosd: this is not the installed binary — Telegram bridge disabled. \
                   A second consumer silently eats the operator's messages. Set \
                   PAOS_ALLOW_BRIDGE=1 if you really mean it.");
        return;
    }
    if !claim_telegram(&cfg) {
        eprintln!("paosd: another process already owns this Telegram bot — bridge disabled \
                   in this instance (that is correct; two consumers lose messages)");
        return;
    }
    eprintln!("paosd: telegram bridge active");
    let inbound = Arc::clone(&conn);
    let c_in = cfg.clone();
    std::thread::spawn(move || inbound_loop(inbound, c_in, embedder));

    std::thread::spawn(move || outbound_loop(conn, cfg));
}

/// Where the installed daemon lives. The LaunchAgent hardcodes this path.
fn installed_paosd() -> std::path::PathBuf {
    std::path::Path::new(&std::env::var("HOME").unwrap_or_default()).join(".local/bin/paosd")
}

/// May THIS binary own the Telegram bridge?
///
/// Pure, and separate from the environment it reads, because the failure it prevents is
/// invisible from inside the process that causes it — the stray daemon looks healthy and
/// the real one goes quiet.
fn may_bridge(
    current: &Option<std::path::PathBuf>,
    allow: &Option<String>,
    installed: &std::path::Path,
) -> bool {
    if allow.as_deref().is_some_and(|v| v == "1") {
        return true;
    }
    match current {
        // Canonicalise both sides: ~/.local/bin is a symlink on some machines, and a
        // string compare would then refuse the real daemon.
        Some(c) => {
            let norm = |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or(p.to_path_buf());
            norm(c) == norm(installed)
        }
        // Cannot tell what we are. Refuse: a wrongly-silent bridge is recoverable by
        // restarting the daemon, a wrongly-active one eats the operator's messages.
        None => false,
    }
}

/// Take a machine-wide advisory lock on this bot token, held for the process lifetime.
///
/// Deliberately leaks the file handle: the lock must last as long as the process, and
/// dropping it here would release it immediately. flock is released by the kernel on
/// exit, so a crashed daemon does not wedge the next one.
fn claim_telegram(cfg: &Config) -> bool {
    use std::os::unix::io::AsRawFd;
    // Key on a hash of the token, not the token itself — a filename should not leak a
    // secret to anyone who can list /tmp.
    let mut h: u64 = 1469598103934665603;
    for b in cfg.token.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    let path = std::env::temp_dir().join(format!("paos-telegram-{h:x}.lock"));
    let Ok(file) = std::fs::OpenOptions::new().create(true).write(true).open(&path) else {
        return true;   // cannot lock -> do not block the operator's only channel
    };
    let ok = unsafe { libc_flock(file.as_raw_fd(), 2 | 4) } == 0;   // LOCK_EX|LOCK_NB
    if ok {
        std::mem::forget(file);
    }
    ok
}

fn lock(c: &Mutex<Connection>) -> std::sync::MutexGuard<'_, Connection> {
    c.lock().unwrap_or_else(|p| p.into_inner())
}

fn now_iso() -> String {
    crate::handlers::now_iso()
}

/// Thread id for a room's forum topic, creating it on first use.
///
/// Reuses the `tg_topics` rows the Python daemon already created, so ad-hocs stays
/// ad-hocs and lobby stays lobby across the cutover — the operator's existing threads
/// keep working rather than everything restarting in General.
/// The Telegram topic title for a room: `# <room> · <repos>`.
///
/// The operator's complaint on 2026-08-01 was that the group is a mess, and this is a
/// concrete piece of it: titles are set once at creation and never revisited, so whether a
/// topic names its project is an accident of what the room had declared that day.
/// `# wave2 · agentic-brain` says whose it is; `# ad-hocs` does not, and both are rooms with
/// a repo declared. The list is read on a phone to choose what to open.
///
/// `questions` is not a room — it is the pseudo-room every escalation is pushed to — so it
/// gets a title that says what it is rather than one that pretends it is a project.
fn topic_title(room: &str, repos: Option<&str>) -> String {
    if room == "questions" {
        return "❓ questions · reply to answer".to_string();
    }
    match repos.map(str::trim).filter(|r| !r.is_empty()) {
        Some(r) => format!("# {room} · {r}"),
        None => format!("# {room}"),
    }
}

/// The title a room's topic SHOULD have right now, from the room's current repos.
fn desired_title(conn: &Connection, room: &str) -> String {
    let repos: Option<String> = conn
        .query_row("SELECT repos FROM rooms WHERE room=?1", [room], |r| r.get(0))
        .ok()
        .flatten();
    topic_title(room, repos.as_deref())
}

fn topic_for(conn: &Connection, cfg: &Config, room: &str) -> Option<i64> {
    if !cfg.is_group {
        return None;   // a direct chat has no topics
    }
    // Look up WITHOUT filtering on closed_ts. Filtering was a duplicate generator: a row
    // marked closed made this create a SECOND Telegram topic with the same name, and the
    // INSERT OR REPLACE below then overwrote the row — leaving the original topic
    // orphaned in the group with nothing pointing at it. The operator ended up with three
    // "ad-hocs" topics that way.
    //
    // A closed topic is reopened instead. Telegram keeps the thread; there is no reason
    // to abandon it, and no API to enumerate topics afterwards to find what we lost.
    if let Ok((t, closed)) = conn.query_row(
        "SELECT thread_id, closed_ts FROM tg_topics WHERE kind='room' AND key=?1",
        [room],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?)),
    ) {
        if closed.is_some() {
            if telegram::reopen_topic(cfg, t).is_ok() {
                let _ = conn.execute(
                    "UPDATE tg_topics SET closed_ts = NULL WHERE kind='room' AND key=?1",
                    [room],
                );
            }
        }
        // Keep the title current. GUARDED ON A STRING COMPARE, not called unconditionally:
        // this runs on the path every mirrored message takes, so an unguarded editForumTopic
        // would be one Telegram API call per message forever. The stored title is what we
        // last set, so a mismatch is the only thing that can need work — in steady state
        // this costs one `!=` and nothing else.
        let want = desired_title(conn, room);
        let have: Option<String> = conn
            .query_row("SELECT title FROM tg_topics WHERE kind='room' AND key=?1", [room],
                       |r| r.get(0))
            .ok()
            .flatten();
        if have.as_deref() != Some(want.as_str()) {
            match telegram::rename_topic(cfg, t, &want) {
                // Record ONLY on success. Storing the intended title after a failed call
                // would make the next pass believe it was done and never retry.
                Ok(_) => {
                    let _ = conn.execute(
                        "UPDATE tg_topics SET title=?2 WHERE kind='room' AND key=?1",
                        rusqlite::params![room, want],
                    );
                }
                Err(e) => eprintln!("paosd: could not retitle topic {t} for {room}: {e}"),
            }
        }
        return Some(t);
    }
    eprintln!("paosd: creating Telegram topic for room {room}");
    // Create it with the SAME title the rename path would give it. These used to disagree:
    // creation used the bare room name while the titles already in the group carried
    // `# room · repos`, so the group's naming depended on when a topic happened to be made.
    let title = desired_title(conn, room);
    match telegram::create_topic(cfg, &title) {
        Ok(id) => {
            // INSERT, not INSERT OR REPLACE. A replace here silently abandons a topic
            // that still exists in the group, and Telegram offers bots no way to list
            // topics afterwards — so the orphan is undiscoverable except by scrolling.
            if let Err(e) = conn.execute(
                "INSERT INTO tg_topics(kind, key, thread_id, title, created_ts) \
                 VALUES('room', ?1, ?2, ?3, ?4)",
                rusqlite::params![room, id, title, now_iso()],
            ) {
                eprintln!("paosd: created topic {id} for {room} but could not record it: {e}");
            }
            Some(id)
        }
        Err(e) => {
            // Falling back to General is better than dropping the message, but say so.
            eprintln!("paosd: could not create topic for {room}: {e}");
            None
        }
    }
}

/// Is a listener — Python or Rust — holding this handle's advisory lock?
///
/// Probing is non-destructive: the file is opened WITHOUT truncation so a successful
/// probe never damages the holder's record. Acquiring the lock means nobody held it.
fn listener_lock_held(name: &str) -> bool {
    let path = paos_store::root()
        .join("listen")
        .join(format!("{}.lock", name.replace('/', "_")));
    let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).open(&path) else {
        return false;   // no lock file at all -> nothing is listening
    };
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    // LOCK_EX|LOCK_NB: success means it was FREE, so no listener.
    let free = unsafe { libc_flock(fd, 2 | 4) } == 0;
    if free {
        unsafe { libc_flock(fd, 8) };   // LOCK_UN — release immediately
    }
    !free
}

extern "C" {
    #[link_name = "flock"]
    fn libc_flock(fd: i32, operation: i32) -> i32;
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Long-poll Telegram. Runs regardless of `may_push`: **inbound is never gated**, or
/// the operator could not open the channel in the first place.
fn inbound_loop(
    conn: Arc<Mutex<Connection>>,
    cfg: Config,
    embedder: Arc<dyn paos_memory::Embedder>,
) {
    let mut offset: i64 = 0;
    loop {
        let off = (offset + 1).to_string();
        let timeout = telegram::LONG_POLL_SECS.to_string();
        let res = telegram::call(
            &cfg,
            "getUpdates",
            &[("timeout", timeout.as_str()), ("offset", off.as_str())],
            telegram::LONG_POLL_SECS + 15,
        );
        match res {
            Ok(body) => {
                // Record that Telegram answered. This is the operator's lifeline: if the
                // bridge wedges, today the only symptom is that his messages stop being
                // answered — which reads exactly like a busy session. `paos doctor` uses
                // this timestamp to say the bridge went quiet.
                {
                    let g = lock(&conn);
                    let _ = g.execute(
                        "INSERT OR REPLACE INTO meta(key, value) VALUES('telegram.last_poll_epoch', ?1)",
                        [now_epoch().to_string()],
                    );
                }
                let updates = telegram::parse_updates(&body);
                if let Some(id) = telegram::max_update_id(&updates) {
                    offset = offset.max(id);
                }
                for u in updates {
                    // AUTHORISATION. Without this, anyone in the group can post to the
                    // bus as the `operator` identity — the one sender peers are told
                    // cannot be impersonated. The Python daemon rejects a real
                    // non-operator user in this very group today.
                    if !cfg.is_authorized(u.from_id) {
                        eprintln!("paosd: ignoring unauthorized update from {:?}", u.from_id);
                        continue;
                    }
                    // Mutable, because answering an escalation now POSTS the answer to
                    // the asking session and the bus allocates its sequence inside a
                    // transaction.
                    let mut g = lock(&conn);
                    let _ = op::mark_operator_seen(&g, &now_iso());
                    if let Some(data) = u.callback_data.as_deref() {
                        handle_callback(&mut g, &cfg, data, u.callback_id.as_deref(),
                                        u.callback_message_id, u.chat_id.as_deref(),
                                        embedder.as_ref());
                        continue;
                    }
                    if let Some(text) = u.text.as_deref() {
                        handle_message(&mut g, &cfg, text, u.thread_id, u.reply_to_message_id,
                                       u.chat_id.as_deref());
                    }
                }
            }
            Err(e) => {
                eprintln!("paosd: getUpdates: {e}");
                // Back off rather than hammering a broken network.
                std::thread::sleep(Duration::from_secs(10));
            }
        }
    }
}

/// A tapped inline button: answer the escalation it belongs to.
///
/// callback_data is `esc:<id>:<option index>`.
/// The persistent control panel: the things worth a glance, one tap away.
/// The panel's own rows, without the wrapper — so a view that adds its OWN buttons can
/// still carry the navigation back. A tap that strands the operator on a screen with no
/// way out but typing a command is how a panel stops being used.
/// What a panel button — or its slash command — shows.
///
/// ONE definition per view, so a button and its command cannot drift into showing
/// different things. They were separate: `/blocked` and `/parked` rendered inline in the
/// command match and had no buttons at all, which is how the panel came to be missing
/// half the bot.
fn view(conn: &Connection, what: &str) -> Option<String> {
    Some(match what {
        "accounts" => op::accounts::render(
            op::accounts::snapshot_local().as_deref().unwrap_or(&[])),
        "digest" => op::digest(conn),
        "who" => fleet(conn),
        "tasks" => crate::tg_tasks::digest(conn),
        "blocked" => {
            let rows = op::open_escalations(conn).unwrap_or_default();
            if rows.is_empty() { "no sessions are blocked".to_string() }
            else { rows.iter().map(|(i, s, q)| format!("#{i} [{s}] {q}"))
                       .collect::<Vec<_>>().join("\n") }
        }
        "parked" => {
            let rows = op::open_parked(conn).unwrap_or_default();
            if rows.is_empty() { "nothing parked".to_string() }
            else { rows.iter().map(|(i, s, n)| format!("#{i} [{s}] {n}"))
                       .collect::<Vec<_>>().join("\n") }
        }
        "health" => paos_memory::health::analyse(conn, 500)
            .map(|r| paos_memory::health::render(&r))
            .unwrap_or_else(|e| format!("health failed: {e}")),
        _ => return None,
    })
}

/// The panel's rows, built from the current state.
///
/// Was a const string, which meant the panel could not show a count and could not mark
/// the active mode — so every button was a question rather than an answer. "tasks" tells
/// you nothing until you tap it; "tasks 2" usually IS the answer.
fn panel_rows(conn: &Connection) -> String {
    let n = |v: i64| if v > 0 { format!(" {v}") } else { String::new() };
    let tasks = n(paos_tasks::query::needs_operator(conn).unwrap_or(0));
    let blocked = n(op::open_escalations(conn).map(|r| r.len() as i64).unwrap_or(0));
    let parked = n(op::open_parked(conn).map(|r| r.len() as i64).unwrap_or(0));
    let mode = op::get_mode(conn);
    // The active mode is MARKED rather than hidden. Mode is the setting most often
    // wrong, and a panel that shows it answers the question without a tap.
    let m = |label: &str, want: op::Mode| {
        if mode == want { format!("• {label}") } else { label.to_string() }
    };
    format!(
        "[{{\"text\":\"📋 needs me\",\"callback_data\":\"panel:digest\"}},\
          {{\"text\":\"🗂 tasks{tasks}\",\"callback_data\":\"panel:tasks\"}}],\
         [{{\"text\":\"⛔ blocked{blocked}\",\"callback_data\":\"panel:blocked\"}},\
          {{\"text\":\"🅿 parked{parked}\",\"callback_data\":\"panel:parked\"}}],\
         [{{\"text\":\"👥 fleet\",\"callback_data\":\"panel:who\"}},\
          {{\"text\":\"🤖 accounts\",\"callback_data\":\"panel:accounts\"}},\
          {{\"text\":\"🩺 health\",\"callback_data\":\"panel:health\"}}],\
         [{{\"text\":\"{here}\",\"callback_data\":\"mode:here\"}},\
          {{\"text\":\"{auto}\",\"callback_data\":\"mode:auto\"}},\
          {{\"text\":\"{away}\",\"callback_data\":\"mode:away\"}}]",
        here = m("🏠 here", op::Mode::Attended),
        auto = m("🤖 auto", op::Mode::Autonomous),
        away = m("✈️ away", op::Mode::Away),
    )
}

fn panel_markup(conn: &Connection) -> String {
    format!("{{\"inline_keyboard\":[{}]}}", panel_rows(conn))
}

fn handle_callback(
    conn: &mut Connection,
    cfg: &Config,
    data: &str,
    callback_id: Option<&str>,
    message_id: Option<i64>,
    chat: Option<&str>,
    embedder: &dyn paos_memory::Embedder,
) {
    // Switching mode from the panel. It is the control the operator touches most — the
    // lobby history is mostly mode flips — and it was the one thing the panel could not
    // do, so changing mode meant leaving the panel to type a command.
    if let Some(want) = data.strip_prefix("mode:") {
        let m = match want {
            "here" => op::Mode::Attended,
            "auto" => op::Mode::Autonomous,
            "away" => op::Mode::Away,
            _ => return,
        };
        if op::set_mode(conn, m, "telegram", &now_iso()).unwrap_or(false) {
            // Ambient, like every other mode change: the fleet must know, but not spend
            // a turn each on being told.
            let _ = post_as_operator(conn, "lobby", "@all",
                &format!("⚙ operator mode → {}", m.as_str()), true);
        }
        if let Some(cb) = callback_id {
            telegram::answer_callback(cfg, cb, &format!("mode: {}", m.as_str()));
        }
        if let Some(mid) = message_id {
            telegram::edit_message_in(cfg, chat, mid, &op::digest(conn),
                                      Some(&panel_markup(conn)));
        }
        return;
    }
    // Panel buttons: a tap gives the current state in place, with no message to scroll
    // back through. The operator asked for usage on a BUTTON rather than as an alert he
    // did not ask for — a number you fetch when you want it beats one pushed at you.
    if let Some(what) = data.strip_prefix("panel:") {
        // Tasks hands off entirely to its own screen — body AND keyboard. Rendering the
        // task body under the PANEL's buttons would show a "pick a repo" prompt with no
        // repos to pick, which is worse than not offering it.
        if what == "tasks" {
            let s = crate::tg_tasks::screen_repos(conn);
            if let Some(cb) = callback_id {
                telegram::answer_callback(cfg, cb, &s.toast);
            }
            if let Some(mid) = message_id {
                telegram::edit_message_in(cfg, chat, mid, &s.body, s.markup.as_deref());
            }
            return;
        }
        let Some(body) = view(conn, what) else { return };
        if let Some(cb) = callback_id {
            telegram::answer_callback(cfg, cb, "…");
        }
        // Edit the panel in place rather than sending another message. The complaint was
        // volume; a panel that replaces itself adds none.
        //
        // The digest is the one view whose LINES are work, so it gets its own buttons on
        // top of the navigation. Without them it is a list of things he cannot do
        // anything about, which is exactly what he reported on 2026-08-02.
        if let Some(mid) = message_id {
            let rows = panel_rows(conn);
            let markup = if what == "digest" {
                crate::tg_digest::markup(conn, true, &rows)
                    .unwrap_or_else(|| panel_markup(conn))
            } else {
                panel_markup(conn)
            };
            telegram::edit_message_in(cfg, chat, mid, &body, Some(&markup));
        }
        return;
    }
    // A tapped DIGEST button. Each one either completes the action or opens the thing
    // that does — none of them merely re-states the count, which is what the digest
    // already did and what made it unactionable from a phone.
    if let Some(what) = data.strip_prefix("dg:") {
        let (body, markup) = match what {
            "props" => crate::tg_digest::proposal_card(conn, None),
            "reap" => (crate::tg_digest::reap(conn, op::poll::now_epoch()), None),
            "parked" => (crate::tg_digest::parked(conn), None),
            _ => return,
        };
        if let Some(cb) = callback_id {
            telegram::answer_callback(cfg, cb, "…");
        }
        if let Some(mid) = message_id {
            // Fall back to the panel rows so an action with no buttons of its own still
            // leaves somewhere to go.
            let m = markup.unwrap_or_else(|| panel_markup(conn));
            telegram::edit_message_in(cfg, chat, mid, &body, Some(&m));
        }
        return;
    }
    // A decision on one memory proposal. `ok` APPLIES it — see `tg_digest::approve` for
    // why the order (plan, write, only then resolve) is not negotiable.
    if let Some(rest) = data.strip_prefix("mp:") {
        let Some((verb, id)) = rest.split_once(':') else { return };
        let Ok(id) = id.parse::<i64>() else { return };
        // `skip` is a CURSOR, not a decision — it resolves nothing and writes nothing.
        let (note, after) = match verb {
            "ok" => (Some(crate::tg_digest::approve(conn, embedder, id)), None),
            "no" => (Some(crate::tg_digest::reject(conn, id)), None),
            "skip" => (None, Some(id)),
            _ => return,
        };
        if let Some(cb) = callback_id {
            telegram::answer_callback(cfg, cb, note.as_deref().unwrap_or("…"));
        }
        // Advance in place. Leaving the decided card on screen is how the same proposal
        // gets approved twice.
        if let Some(mid) = message_id {
            let (body, markup) = crate::tg_digest::proposal_card(conn, after);
            let m = markup.unwrap_or_else(|| panel_markup(conn));
            let text = match &note {
                Some(n) => format!("{n}\n\n{body}"),
                None => body,
            };
            telegram::edit_message_in(cfg, chat, mid, &text, Some(&m));
        }
        return;
    }
    // A tapped task button. Acting as the operator, through the same `set_state` the
    // CLI and the board use — so the transition rules and the note trail are identical
    // and there is no third place that can disagree about what a close means.
    // The whole `task:` namespace — navigation and actions — is owned by tg_tasks, so
    // this stays a delegation rather than a second copy of the task rules.
    if let Some(rest) = data.strip_prefix("task:") {
        let screen = crate::tg_tasks::callback(conn, rest, &now_iso());
        if let Some(cb) = callback_id {
            telegram::answer_callback(cfg, cb, &screen.toast);
        }
        // Edit in place: a drill-down that posts a new message per step buries the thing
        // you were reading, and a stale list is how you approve the same item twice.
        if let Some(mid) = message_id {
            telegram::edit_message_in(cfg, chat, mid, &screen.body, screen.markup.as_deref());
        }
        return;
    }
    let parts: Vec<&str> = data.split(':').collect();
    if parts.len() != 3 || parts[0] != "esc" {
        return;
    }
    let (Ok(id), Ok(idx)) = (parts[1].parse::<i64>(), parts[2].parse::<usize>()) else {
        return;
    };
    let options = op::escalation_options(conn, id);
    let Some(choice) = options.get(idx).cloned() else { return };
    let answered = op::answer(conn, id, &choice, &now_iso()).unwrap_or(false);
    if let Some(cb) = callback_id {
        telegram::answer_callback(cfg, cb, if answered { &choice } else { "already closed" });
    }
    if answered {
        if let Some(mid) = message_id {
            // Stamp the message so the chat carries the decision — an audit trail.
            telegram::edit_message_in(cfg, chat, mid, &format!("✅ answered: {choice}"), None);
        }
    }
}

/// The slash-command menu published on startup.
pub const BOT_COMMANDS: &[(&str, &str)] = &[
    ("digest", "what needs you right now"),
    ("who", "fleet — who is busy or blocked"),
    ("tasks", "the work queue — what needs your decision"),
    ("blocked", "sessions waiting on a decision"),
    ("parked", "deferred decisions"),
    ("accounts", "Claude accounts and usage"),
    ("panel", "control panel — accounts, what needs you, fleet"),
    ("topics", "which Telegram topic paos uses for each room"),
    ("health", "memory hygiene report"),
    ("switch", "rotate to the least-used Claude account"),
    ("here", "attended — stay quiet here"),
    ("auto", "autonomous — hands off, still quiet"),
    ("away", "away — ping me here until I say /here"),
    ("help", "all commands"),
    ("keyboard", "reinstall the always-on keyboard"),
];

/// Turn one operator message into an action.
fn handle_message(
    conn: &mut Connection,
    cfg: &Config,
    text: &str,
    thread_id: Option<i64>,
    reply_to: Option<i64>,
    chat: Option<&str>,
) {
    let t = text.trim();
    if t.is_empty() {
        return;
    }
    // A quote-reply routes to whoever originated the quoted message: an escalation gets
    // answered, a session gets steered. Without this the reply just broadcasts.
    if let Some(mid) = reply_to {
        if let Some(eid) = op::escalation_by_message_id(conn, mid) {
            if op::answer(conn, eid, t, &now_iso()).unwrap_or(false) {
                reply(cfg, chat, thread_id, &format!("✓ answer delivered to escalation #{eid}"));
            } else {
                reply(cfg, chat, thread_id, "that escalation is already closed");
            }
            return;
        }
        // A COMMAND typed as a reply is a command, not chatter.
        //
        // This branch used to swallow it: quote-replying to a session's message and
        // typing `/tasks` relayed the literal string to that session and returned, so the
        // command never ran and nothing said so. Recorded on the bus 2026-08-03 as
        // `operator -> @dapper-shrike: 📱 operator: /tasks`. It also explains why
        // commands "worked sometimes" — they work from a new message and vanish from a
        // reply, and in a topic where a session is talking to you, replying is the
        // natural gesture.
        //
        // The escalation branch above KEEPS priority on purpose: an answer is free text,
        // and "[answer to #45] public now" is a legitimate reply that happens to start
        // with no slash. Only this branch defers.
        if !is_command(t) {
            if let Some(session) = op::session_by_message_id(conn, mid) {
                let _ = post_as_operator(conn, "lobby", &format!("@{session}"),
                                        &format!("📱 operator: {t}"), false);
                return;
            }
        }
    }
    // Telegram appends `@botname` to a command whenever it is sent in a group — which
    // this is, and which is where every one of these is typed. Without stripping it,
    // `/tasks@example_bot` matches nothing, falls through to the chatter path at the
    // bottom of this function, and is BROADCAST TO THE WHOLE FLEET as an operator
    // message. Observed 2026-08-02 with `/tasks@example_bot`, and visible in the lobby
    // history for `/who@example_bot` well before that: the command silently did nothing
    // and the fleet got told about it.
    let cmd = t
        .split_whitespace()
        .next()
        .unwrap_or("")
        .split('@')
        .next()
        .unwrap_or("")
        .to_lowercase();
    // A phone keyboard button sends its LABEL as plain text — "📊 Digest", not "/digest".
    // The operator's keyboard is theirs, not something this code defines, so its labels
    // cannot be enumerated here. Map the label back to a command instead: any emoji-
    // prefixed label whose word is a known command becomes that command, and a command
    // added later works as a label with no further change.
    //
    // Enumerating labels was the wrong shape and it showed — I guessed them from the
    // inline markup, shipped, and "📊 Digest" still fell through to the fleet. Three
    // rounds of the same whack-a-mole.
    let cmd = if cmd.starts_with('/') { cmd } else {
        command_from_label(t).unwrap_or(cmd)
    };
    match cmd.as_str() {
        "/away" | "/here" | "/auto" => {
            let m = match cmd.as_str() {
                "/away" => op::Mode::Away,
                "/here" => op::Mode::Attended,
                _ => op::Mode::Autonomous,
            };
            if op::set_mode(conn, m, "telegram", &now_iso()).unwrap_or(false) {
                let _ = post_as_operator(conn, "lobby", "@all",
                    &format!("⚙ operator mode → {}", m.as_str()), true);
            }
            reply(cfg, chat, thread_id, &format!("mode: {}", m.as_str()));
            return;
        }
        "/digest" => {
            let mut d = op::digest(conn);
            // A weekly cap about to hit stops every session at once, so it belongs in
            // the same "what needs you" view as a blocked session.
            if let Some(list) = op::accounts::snapshot_local() {
                if let Some(worst) = list.iter().find(|a| a.is_critical()) {
                    d.push_str(&format!("\n• 🔴 Claude {} at {:.0}% of its weekly limit",
                                        worst.slot, worst.shown_7d()));
                }
            }
            let markup = crate::tg_digest::markup(conn, false, "");
            reply_with_markup(cfg, chat, thread_id, &d, markup.as_deref());
            return;
        }
        "/who" => { reply(cfg, chat, thread_id, &fleet(conn)); return; }
        // The work queue, phone-shaped: what needs a decision, with a button on each,
        // then the columns as a single counts line. Not a board — five columns do not
        // fit on a phone, and the question there is never "show me the grid".
        "/tasks" => {
            let body = crate::tg_tasks::digest(conn);
            let markup = crate::tg_tasks::markup(conn);
            reply_with_markup(cfg, chat, thread_id, &body, markup.as_deref());
            return;
        }
        "/topics" => {
            let mut out = String::from("rooms → Telegram topics paos knows:\n");
            if let Ok(mut st) = conn.prepare(
                "SELECT key, thread_id, closed_ts FROM tg_topics WHERE kind='room' ORDER BY key") {
                if let Ok(rows) = st.query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?))
                }) {
                    for (k, t, c) in rows.flatten() {
                        out.push_str(&format!("  {k} → {t}{}\n",
                                              if c.is_some() { " (closed)" } else { "" }));
                    }
                }
            }
            out.push_str("\nAny OTHER topic with these names is an orphan — paos will not \
                          post there. Use /adopt <room> inside a topic to make it the one.");
            reply(cfg, chat, thread_id, &out);
            return;
        }
        "/adopt" => {
            let Some(tid) = thread_id else {
                reply(cfg, chat, thread_id, "run /adopt <room> INSIDE the topic you want to keep");
                return;
            };
            match t.split_whitespace().nth(1) {
                Some(room) => {
                    let _ = conn.execute("DELETE FROM tg_topics WHERE kind='room' AND key=?1", [room]);
                    let _ = conn.execute(
                        "INSERT INTO tg_topics(kind, key, thread_id, title, created_ts) \
                         VALUES('room', ?1, ?2, ?1, ?3)",
                        rusqlite::params![room, tid, now_iso()]);
                    reply(cfg, chat, thread_id, &format!(
                        "✓ this topic is now the one for '{room}'. Delete any other topic \
                         with that name — paos will not use it."));
                }
                None => reply(cfg, chat, thread_id, "usage: /adopt <room>"),
            }
            return;
        }
        "/panel" => {
            let body = op::accounts::render(
                op::accounts::snapshot_local().as_deref().unwrap_or(&[]));
            reply_with_markup(cfg, chat, thread_id, &body, Some(&panel_markup(conn)));
            return;
        }
        "/blocked" => {
            reply(cfg, chat, thread_id, &view(conn, "blocked").unwrap_or_default());
            return;
        }
        "/parked" => {
            reply(cfg, chat, thread_id, &view(conn, "parked").unwrap_or_default());
            return;
        }
        "/say" => {
            let parts: Vec<&str> = t.splitn(3, char::is_whitespace).collect();
            match (parts.get(1), parts.get(2)) {
                (Some(tok), Some(body)) => match op::resolve_session(conn, tok) {
                    Some(session) => {
                        let _ = post_as_operator(conn, "lobby", &format!("@{session}"),
                                                 &format!("📱 operator: {}", body.trim()), false);
                        reply(cfg, chat, thread_id, &format!("✓ sent to {session}"));
                    }
                    None => reply(cfg, chat, thread_id,
                                  &format!("no session matches {tok} — /who lists them")),
                },
                _ => reply(cfg, chat, thread_id, "usage: /say <session> <text>"),
            }
            return;
        }
        "/health" => {
            reply(cfg, chat, thread_id, &view(conn, "health").unwrap_or_default());
            return;
        }
        "/accounts" => {
            reply(cfg, chat, thread_id, &accounts_report());
            return;
        }
        "/switch" => {
            // Rotate to whichever account has the most weekly headroom. Doing this from
            // the phone matters: hitting a weekly cap stops every session at once.
            match op::accounts::snapshot_local() {
                // The SAME picker the CLI and the auto-switch poller use. This called
                // `least_used`, which was min(seven_day) with no exclusions — it could
                // offer an account that was itself weekly-exhausted or over TARGET_MAX,
                // and the operator would be told from their phone that it had switched.
                // An explicit /switch is deliberate, so switch_at is forced to 0 and the
                // cooldown is waived; every other rule still applies.
                Some(list) => match op::accounts::decide_switch(
                    &list,
                    &op::accounts::SwitchConfig { switch_at: 0.0, cooldown: 0,
                                                  ..Default::default() },
                    0, 0,
                ) {
                    (None, why) => {
                        // Says WHY rather than "already on X": the useful answer when
                        // every alternative is exhausted is which rule excluded them.
                        reply(cfg, chat, thread_id, &format!("not switching: {why}"));
                    }
                    (Some(slot), _why) => {
                        // In-process, through the ONE switcher. This shelled
                        // `claude-acct use`, so the re-stash of the outgoing credential
                        // and the rollback on a failed identity write lived in Python for
                        // this caller and in Rust for the others.
                        match op::poll::switch_now(&slot, op::poll::now_epoch()) {
                            Ok(()) => reply(cfg, chat, thread_id, &format!("✓ switched to {slot}\n\n{}",
                                             accounts_report())),
                            Err(e) => reply(cfg, chat, thread_id, &format!("switch failed: {e}")),
                        }
                    }
                },
                None => reply(cfg, chat, thread_id, "Claude account tooling not available here"),
            }
            return;
        }
        // Installing the keyboard is its own command because a reply keyboard is only
        // replaced by a message that carries one — there is no way to push a new one at a
        // client that is not asking. One tap here and the stale keyboard is gone.
        "/keyboard" => {
            reply_with_markup(cfg, chat, thread_id,
                              "keyboard installed — every command has a key",
                              Some(&keyboard_markup()));
            return;
        }
        "/help" | "/start" => {
            let body = BOT_COMMANDS.iter()
                .map(|(c, d)| format!("/{c} — {d}"))
                .collect::<Vec<_>>().join("\n");
            // Carried here too, so anyone who ever types /start gets a current keyboard
            // without knowing /keyboard exists.
            reply_with_markup(cfg, chat, thread_id, &format!("paos operator channel\n\n{body}\n\n\
                Reply to a session's message to steer it; reply to a question to answer it.\n\
                @<session> <text> targets one session."),
                Some(&keyboard_markup()));
            return;
        }
        _ => {}
    }
    // `@handle text` targets one session — resolved against live handles, so the
    // operator can type a short name.
    if let Some(rest) = t.strip_prefix('@') {
        if let Some((tok, body)) = rest.split_once(char::is_whitespace) {
            if let Some(session) = op::resolve_session(conn, tok) {
                let _ = post_as_operator(conn, "lobby", &format!("@{session}"),
                                         &format!("📱 operator: {}", body.trim()), false);
                return;
            }
            reply(cfg, chat, thread_id, &format!("no session matches @{tok} — /who lists them"));
            return;
        }
    }
    // A bare mode LABEL — what a phone reply-keyboard sends, as opposed to `/away`.
    //
    // Observed live on 2026-07-31: the operator's phone sent "✈️ Away", then "🤖 Auto", as
    // plain TEXT. Neither is a panel button label here, so both fell through to the
    // broadcast below, and two things went wrong at once — neither visible to the
    // operator. The mode did NOT change (it still read `attended` afterwards), and because
    // the fallthrough posts with ambient=false — correct in general, since anything the
    // human TYPES must reach sessions immediately — every listening session in lobby woke
    // and spent a turn on it. Twice, minutes apart. That is precisely the cost the ambient
    // flag exists to prevent, arriving through a different door.
    //
    // So do what the operator meant: set the mode, and announce it AMBIENT like every
    // other mode change.
    if let Some(m) = mode_from_label(t) {
        if op::set_mode(conn, m, "telegram", &now_iso()).unwrap_or(false) {
            let _ = post_as_operator(conn, "lobby", "@all",
                &format!("⚙ operator mode → {}", m.as_str()), true);
        }
        reply(cfg, chat, thread_id, &format!("mode: {}", m.as_str()));
        return;
    }
    // Plain text goes to the room whose topic it was typed in. Falling back to a fixed
    // room would post where most sessions are not.
    let room = thread_id
        .and_then(|tid| room_for_thread(conn, tid))
        // No topic = "everyone". lobby is the room every session joins, so a message in
        // General reaches the whole fleet; a message in a room's topic reaches that room.
        .unwrap_or_else(|| "lobby".to_string());
    let _ = post_as_operator(conn, &room, "@all", &format!("📱 operator: {t}"), false);
    // Then say so if it landed nowhere. AFTER the post, not instead of it: the message is
    // still stored and still readable via `paos bus log`, so refusing to post would destroy
    // information. The only thing missing was the operator KNOWING.
    // Same join, same reason: a member row survives its session, so counting rows would
    // report a room full of readers that all went home hours ago.
    let readers: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM members m JOIN sessions s ON s.name = m.name \
             WHERE m.room = ?1 AND s.ended_ts IS NULL",
            [&room],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if let Some(w) = unread_room_warning(&room, readers, &rooms_with_readers(conn)) {
        reply(cfg, chat, thread_id, &w);
    }
}

/// Is this bus message addressed to the OPERATOR?
///
/// Telegram is how a session reaches the human, not a mirror of the bus. Everything else —
/// peers coordinating, status, room chatter — belongs in the dashboard and `paos bus`,
/// and putting it on his phone makes the one channel that must stay signal into noise.
///
/// `@all` is deliberately NOT a match. A broadcast is aimed at the fleet, and treating it
/// as operator-addressed is how "everything in every room" ends up on the phone: `@all` is
/// the most common target on the bus by a wide margin.
fn targets_operator(target: &str) -> bool {
    // Multi-target is `@a__b__c`. Split, strip each, and require an exact match — a
    // substring test would make `@operator-relay` or `@my-operator` reach the phone.
    target
        .trim()
        .trim_start_matches('@')
        .split("__")
        .any(|p| p.trim().trim_start_matches('@').eq_ignore_ascii_case("operator"))
}

/// A message that is nothing but a mode label, however the phone decorated it.
///
/// Deliberately strict: the ENTIRE message must be one word once emoji and punctuation
/// are stripped. "Away" and "✈️ Away" set the mode; "away for lunch, back at 3" is a
/// message to the fleet and stays one. Getting that boundary wrong is worse than the bug
/// it fixes — a false positive silently eats a message meant for the fleet AND changes
/// the mode behind the operator's back, two invisible failures instead of one.
fn mode_from_label(t: &str) -> Option<op::Mode> {
    // Split FIRST, then strip decoration from each piece. Stripping before splitting
    // FUSES words, so "aw ay" would collapse to "away" and set the mode from a message
    // that says no such thing.
    let words: Vec<String> = t
        .split_whitespace()
        .map(|w| w.chars().filter(char::is_ascii_alphabetic).collect::<String>().to_lowercase())
        .filter(|w| !w.is_empty())
        .collect();
    let [word] = words.as_slice() else { return None };
    match word.as_str() {
        "away" => Some(op::Mode::Away),
        "auto" | "autonomous" => Some(op::Mode::Autonomous),
        "here" | "attended" => Some(op::Mode::Attended),
        // Deliberately NOT "back", "brb", "ok" and friends. They plausibly mean a mode,
        // and that is exactly the problem — only words with no other reading as a
        // standalone message qualify.
        _ => None,
    }
}

/// Would this text be handled as a command?
///
/// Used to decide whether a QUOTE-REPLY is a command or a message to relay. Kept as one
/// predicate so the answer cannot differ from what the parser below actually does.
fn is_command(t: &str) -> bool {
    t.trim_start().starts_with('/') || command_from_label(t).is_some()
        || mode_from_label(t).is_some()
}

/// A phone keyboard button label, e.g. "📊 Digest" or "👥 Fleet", turned into its command.
///
/// Requires BOTH an emoji and a word that names a real command. The emoji is the entire
/// safety margin: it is on every keyboard label and absent from prose, so a message
/// reading "tasks" or "fleet" stays a message to the fleet. `mode_from_label` next door
/// documents why that boundary matters — a false positive silently eats something meant
/// for other sessions AND does something nobody asked for.
///
/// The command list is the source of truth rather than a hand-written label list. The
/// hand-written version needed three rounds and still missed "📊 Digest".
fn command_from_label(t: &str) -> Option<String> {
    // Any non-ASCII character counts as the decoration, and it must NOT also demand
    // `!is_alphabetic()`. Unicode calls 🅿 (U+1F17F, SQUARED LATIN CAPITAL LETTER P)
    // alphabetic, and so does Rust — so "🅿 parked" failed this guard and the parked key
    // silently broadcast its own label to the fleet. Every squared-letter emoji (🅰 🅱 🅾
    // 🆎 …) has the same property, so this was a trap set for whoever picked the next
    // icon, not a one-off.
    //
    // Dropping the alphabetic clause costs nothing: word extraction keeps only ASCII
    // letters, so a non-ASCII character can never be part of the matched word. The real
    // safety is below — the word has to BE a command name — and pure ASCII prose still
    // returns None here, which is what keeps a message reading "tasks" a message.
    if t.is_ascii() {
        return None;
    }
    let words: Vec<String> = t
        .split_whitespace()
        .map(|w| w.chars().filter(char::is_ascii_alphabetic).collect::<String>().to_lowercase())
        .filter(|w| !w.is_empty())
        .collect();
    let joined = words.join(" ");
    // Labels whose wording does not match the command they mean.
    let name = match joined.as_str() {
        "fleet" => "who",
        "needs me" => "digest",
        other => other,
    };
    BOT_COMMANDS
        .iter()
        .find(|(c, _)| *c == name)
        .map(|(c, _)| format!("/{c}"))
}

/// The icon a command wears on the always-on keyboard.
///
/// Unknown commands deliberately get a bullet rather than being skipped: a command added
/// later must still reach the keyboard, because "the keyboard is incomplete" is the
/// complaint this whole thing exists to end. Cosmetics are not worth a missing button.
fn icon(cmd: &str) -> &'static str {
    match cmd {
        "digest" => "📋", "who" => "👥", "tasks" => "🗂", "blocked" => "⛔",
        "parked" => "🅿", "accounts" => "🤖", "panel" => "🎛", "topics" => "🧵",
        "health" => "🩺", "switch" => "🔄", "here" => "🏠", "auto" => "🤝",
        "away" => "✈️", "help" => "❓", "keyboard" => "⌨️",
        _ => "•",
    }
}

/// The persistent reply keyboard — the one always on screen.
///
/// Reported 2026-08-03: "The keyboard is still not complete … It is missing tasks and
/// smth else". It was incomplete because NOTHING HERE HAD EVER SENT ONE. The keyboard on
/// his phone came from the retired Python bot and has sat there ever since, because a
/// reply keyboard lives on the CLIENT until something replaces it — which is also why its
/// labels kept arriving as plain text that matched no command.
///
/// So it is generated from `BOT_COMMANDS`, the same table `command_from_label` resolves
/// against. Completeness is then a property rather than a thing to remember: a command
/// added tomorrow gets a key, and the pair of tests asserts both directions — every
/// command has a key, and every key resolves to a command.
fn keyboard_markup() -> String {
    let keys: Vec<String> = BOT_COMMANDS
        .iter()
        .map(|(c, _)| format!("{{\"text\":\"{} {c}\"}}", icon(c)))
        .collect();
    let rows: Vec<String> = keys
        .chunks(3)
        .map(|r| format!("[{}]", r.join(",")))
        .collect();
    format!(
        "{{\"keyboard\":[{}],\"resize_keyboard\":true,\"is_persistent\":true}}",
        rows.join(",")
    )
}

/// Which room a forum topic belongs to — the inverse of `topic_for`.
fn room_for_thread(conn: &Connection, thread_id: i64) -> Option<String> {
    conn.query_row(
        "SELECT key FROM tg_topics WHERE kind='room' AND thread_id=?1",
        [thread_id],
        |r| r.get(0),
    )
    .ok()
}

/// Rooms that a LIVE session is actually in, most-populated first.
///
/// THE JOIN IS THE WHOLE POINT, and my first version did not have it. I read in
/// `paos-presence` that membership is reaped when a session ends and took it as liveness.
/// The reap is `DELETE FROM members WHERE name NOT IN (SELECT name FROM sessions)` — it
/// fires when the SESSION ROW disappears, and ending a session sets `ended_ts` and keeps
/// the row. So membership outlives the session, by days.
///
/// Measured on the live database the moment I checked instead of assuming:
///   motion-fleet               3 members, 0 live
///   motion-qbo-e2e-followups   2 members, 0 live
///   lobby                     16 members, 9 live
///
/// Which means the warning this feeds would have stayed SILENT on precisely the rooms it
/// exists to catch — a topic whose sessions have all gone home is the likeliest place for
/// the operator to type into the void. A false negative here is invisible; that is what
/// makes it worth the join.
fn rooms_with_readers(conn: &Connection) -> Vec<(String, i64)> {
    // No `HAVING c > 0`: I wrote one, then a sabotage run showed relaxing it changed
    // nothing, because GROUP BY cannot produce an empty group. A guard that can never fire
    // reads like a real one to the next person.
    conn.prepare(
        "SELECT m.room, COUNT(*) c FROM members m JOIN sessions s ON s.name = m.name \
         WHERE s.ended_ts IS NULL GROUP BY m.room ORDER BY c DESC, m.room",
    )
    .and_then(|mut s| {
        s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map(|it| it.filter_map(Result::ok).collect())
    })
    .unwrap_or_default()
}

/// Warn the operator when the topic they just typed in reaches NOBODY.
///
/// `post_as_operator` does `INSERT OR IGNORE INTO rooms`, so typing in a topic whose room
/// does not exist silently CREATES an empty room and posts into it. No session is in it, so
/// the message is simply gone — and nothing anywhere said so. The operator's own words for
/// the state this produces: "right now it's actually a mess".
///
/// Measured 2026-08-01: the group's `questions` topic (thread 498) maps to a room with no
/// row in `rooms`, no members and no messages.
///
/// I FIRST READ THAT AS LITTER AND IT IS THE OPPOSITE — `questions` is deliberate, the
/// pseudo-room this file uses to get one Telegram topic for every escalation push (see the
/// escalation loop's `topic_for(.., "questions")`). Which makes the defect worse, not
/// better: that topic is exactly where the operator would type an ANSWER, and plain text
/// there routes to a room no session is in. The intended path is a quote-reply to the
/// escalation message, and nothing said so.
///
/// This is the same defect class as the three fixed earlier today — a channel to a human
/// that fails without saying so — except pointing the other way, from the human to us. It
/// is worse in that direction: a session that gets no reply eventually asks again, whereas
/// the operator has no way to discover the message was never delivered.
fn unread_room_warning(room: &str, readers: i64, live: &[(String, i64)]) -> Option<String> {
    if readers > 0 {
        return None;
    }
    // The escalation topic gets its own answer, because "type in another topic" is exactly
    // the wrong advice there — the operator is trying to answer a question, and the room
    // they need is the asking session's, which only the escalation itself knows.
    if room == "questions" {
        return Some(
            "⚠ THAT ANSWERED NOTHING. This topic carries escalations OUT; plain text typed \
             here goes to a room no session is in.\n\
             To answer: REPLY to the ❓ message itself (quote it) and it reaches the session \
             that asked. Tap an option button if the question offered any."
                .to_string(),
        );
    }
    let mut w = format!(
        "⚠ NOBODY RECEIVED THAT. No live session is in '{room}', so the message was stored \
         and will not be read by anyone.\n"
    );
    if live.is_empty() {
        w.push_str("No room currently has a live session in it. `/who` shows the fleet.");
    } else {
        w.push_str("Rooms with live sessions right now:\n");
        for (r, n) in live.iter().take(8) {
            w.push_str(&format!("  {r} — {n} session{}\n", if *n == 1 { "" } else { "s" }));
        }
        w.push_str("Type in one of those topics, or reply to a session's message to steer it.");
    }
    Some(w)
}

fn accounts_report() -> String {
    match op::accounts::snapshot_local() {
        Some(list) => op::accounts::render(&list),
        None => "Claude account tooling not available here".to_string(),
    }
}

/// One phone line per session — and the status TRUNCATED, which is the whole point.
///
/// `status` is free text a session writes about itself, and sessions write PARAGRAPHS
/// there: on this machine the longest live one is ~1,800 characters, and several run past
/// 400. Twenty of those is ~10,000 characters, which `chunk_text` then splits into three
/// consecutive 3,900-character messages. Nothing errors and nothing is dropped — the
/// operator simply gets a wall of text where he asked "who is up?", which is the exact
/// complaint that made Telegram brevity a rule in the first place. The dashboard and
/// `paos bus who` still carry the full status; this is the phone view.
fn fleet(conn: &Connection) -> String {
    let mut st = match conn.prepare(
        "SELECT name, COALESCE(status,'') FROM sessions WHERE ended_ts IS NULL \
         ORDER BY last_seen DESC LIMIT 20") {
        Ok(s) => s,
        Err(_) => return "could not read the fleet".into(),
    };
    let rows: Vec<String> = st
        .query_map([], |r| Ok(format!("• {} — {}", r.get::<_, String>(0)?,
            { let s: String = r.get(1)?;
              if s.trim().is_empty() { "(idle)".into() } else { one_line(&s, 90) } })))
        .map(|it| it.filter_map(Result::ok).collect())
        .unwrap_or_default();
    if rows.is_empty() { "no sessions known".into() } else { rows.join("\n") }
}

/// Collapse to a single line and cut to `max` CHARACTERS.
///
/// Chars, not bytes: statuses carry em-dashes and arrows, and a byte slice through one is
/// a panic, not a truncation.
fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    flat.chars().take(max.saturating_sub(1)).collect::<String>().trim_end().to_string() + "…"
}

/// Reply in the topic the operator typed in, so answers stay where they asked.
/// Answer where the question was asked.
///
/// `chat` is the chat the incoming update arrived in. Passing it through is what makes
/// the bot usable in a DM: before 2026-08-02 every reply went to the configured group, so
/// a command typed in the DM ran and answered somewhere the operator was not looking.
/// Reply WITH BUTTONS, same routing.
///
/// `/panel` was the one command that reached past this and called the raw sender, so it
/// answered into the configured group no matter where it was invoked. Reported
/// 2026-08-03: "I click panel menu in the bot and it goes to general in the group" —
/// exactly right, and the only command it could happen to, because every other arm went
/// through `reply`. That is the argument for the helper rather than the one-line fix: a
/// command arm should have no reason to name a chat at all.
fn reply_with_markup(
    cfg: &Config,
    chat: Option<&str>,
    thread_id: Option<i64>,
    text: &str,
    markup: Option<&str>,
) {
    let _ = telegram::send_to(cfg, chat, text, false, thread_id, markup);
}

fn reply(cfg: &Config, chat: Option<&str>, thread_id: Option<i64>, text: &str) {
    let _ = telegram::send_to(cfg, chat, text, false, thread_id, None);
}


/// Post as the `operator` identity — the one sender a peer cannot impersonate, because
/// sessions never choose their own handle.
pub fn post_as_operator(
    conn: &Connection,
    room: &str,
    target: &str,
    text: &str,
    ambient: bool,
) -> rusqlite::Result<i64> {
    let ts = now_iso();
    conn.execute(
        "INSERT OR IGNORE INTO rooms(room, created_ts) VALUES(?1, ?2)",
        rusqlite::params![room, ts],
    )?;
    let seq: i64 = conn.query_row(
        "SELECT COALESCE(MAX(seq),0)+1 FROM messages WHERE room=?1",
        [room],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT INTO messages(room, seq, ts, sender, target, text, urgent, ambient) \
         VALUES(?1, ?2, ?3, 'operator', ?4, ?5, 0, ?6)",
        rusqlite::params![room, seq, ts, target, text, ambient as i64],
    )?;
    Ok(seq)
}

/// Escalations and the bus mirror. Both gated on `may_push`.
fn outbound_loop(conn: Arc<Mutex<Connection>>, cfg: Config) {
    telegram::set_my_commands(&cfg, BOT_COMMANDS);
    let mut limiter = Limiter::default();
    // Start ready to sweep so a problem present at boot is reported immediately.
    let mut last_supervise = std::time::Instant::now()
        - Duration::from_secs(SUPERVISE_EVERY_SECS);
    let mut alerted: std::collections::HashSet<String> = Default::default();
    // Seeded from the CURRENT mode, not from a default: starting at `attended` would make
    // the first tick after a restart look like a fresh transition into away and
    // re-escalate every blocked session the operator had already seen.
    let mut prev_mode = { let g = lock(&conn); op::get_mode(&g) };
    loop {
        std::thread::sleep(Duration::from_secs(2));

        // UNCONDITIONAL. These are local bookkeeping — detecting deaf sessions, recording
        // usage, deciding whether anything is wrong. They must not depend on the phone
        // channel being open, because the channel is CLOSED in attended mode (the normal
        // case) and a `continue` here silently disabled all of it: no stale/deaf flagging,
        // no usage samples, no doctor alerts, no outbox drain. Only the SENDING is gated,
        // and each of these checks that for itself before it speaks.
        sample_usage(&conn);
        supervise_and_alert(&conn, &cfg, &mut last_supervise, &mut alerted);
        maybe_digest(&conn, &cfg);
        // Above the gate, necessarily: the transition it watches for is attended -> away,
        // and in attended mode `may_push` is false — so a sweep placed below the gate
        // could never observe the only transition it exists for.
        {
            let g = lock(&conn);
            let now_mode = op::get_mode(&g);
            let was = std::mem::replace(&mut prev_mode, now_mode);
            match op::sweep_blocked_on_away(&g, was, now_mode, &now_iso()) {
                Ok(names) => for n in names {
                    eprintln!("paosd: away sweep escalated blocked session {n}");
                },
                Err(e) => eprintln!("paosd: away sweep: {e}"),
            }
        }

        let open = {
            let g = lock(&conn);
            if !op::may_push(&g, now_epoch()) {
                continue;
            }
            op::unpushed(&g).unwrap_or_default()
        };
        for (id, session, question) in open {
            let text = telegram::neutralize_mentions(
                &format!("❓ {question}\n\n— {session}"),
                cfg.operator_username.as_deref(),
            );
            let tid = { let g = lock(&conn); topic_for(&g, &cfg, "questions") };
            let markup = { let g = lock(&conn);
                           telegram::options_markup(id, &op::escalation_options(&g, id)) };
            match telegram::send_with_markup(&cfg, &text, false, tid, markup.as_deref()) {
                Ok(mid) => {
                    let g = lock(&conn);
                    let _ = op::mark_pushed(&g, id, mid.map(|m| m.to_string()).as_deref());
                    // Record the id so a quote-reply can find this escalation.
                    if let Some(m) = mid {
                        let _ = op::set_escalation_message_id(&g, id, m);
                    }
                }
                Err(e) => eprintln!("paosd: escalation push: {e}"),
            }
        }
        drain_outbox(&conn, &cfg);
        mirror(&conn, &cfg, &mut limiter);
    }
}

/// Flag stale/deaf sessions and PROACTIVELY tell the operator about things they would
/// otherwise have to remember to check.
///
/// The rule for what earns an unprompted push: it must be something that silently stops
/// work. A deaf session ignores every message addressed to it while looking healthy; a
/// maxed weekly account stops every session at once. Both were previously discovered by
/// the operator noticing something was wrong, which is the failure mode to remove.
/// Record a usage sample, at most hourly.
///
/// The accounts view could say "100% of the weekly window is gone" but never WHEN it
/// went, which is the part that would let the operator change anything. Sampling gives a
/// climb rate and a rough correlation with the sessions that were alive at the time.
///
/// It is a correlation, not attribution — Anthropic reports per-account totals and no
/// session tells paos its token spend. Anywhere this is surfaced it must say so, or it
/// becomes a confident accusation of the wrong session.
fn sample_usage(conn: &Arc<Mutex<Connection>>) {
    const HOUR: i64 = 3600;
    const KEY: &str = "usage.last_sample_epoch";
    let now = now_epoch();
    {
        let g = lock(conn);
        let last: i64 = g
            .query_row("SELECT value FROM meta WHERE key=?1", [KEY], |r| r.get::<_, String>(0))
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        if now - last < HOUR {
            return;
        }
    }
    let Some(list) = op::accounts::snapshot_local() else {
        return;   // unreadable usage is not a zero sample; recording 0 would invent a drop
    };
    let g = lock(conn);
    for a in &list {
        // Sample only what was actually MEASURED. An unpollable account has no windows,
        // and writing 0 for it invents a drop to zero in the churn graph — the same
        // reason `snapshot` returning None skips the whole sample above.
        let (Some(five), Some(seven)) = (a.five_hour, a.seven_day) else { continue };
        let _ = g.execute(
            "INSERT OR REPLACE INTO usage_samples(ts, slot, five_hour, seven_day) \
             VALUES(?1, ?2, ?3, ?4)",
            rusqlite::params![now, a.slot, five, seven],
        );
    }
    // 60 days is plenty for "what happened this week" and keeps the table trivial.
    let _ = g.execute("DELETE FROM usage_samples WHERE ts < ?1", [now - 60 * 86_400]);
    let _ = g.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
        rusqlite::params![KEY, now.to_string()],
    );
}

/// At most one queue digest per day, and only when something is pending.
///
/// Sent SILENTLY: an unreviewed proposal is never blocking work, and the point is to be
/// visible tomorrow morning, not to interrupt tonight.
fn maybe_digest(conn: &Arc<Mutex<Connection>>, cfg: &Config) {
    const DAY: i64 = 86_400;
    const KEY: &str = "digest.last_sent_epoch";
    let now = now_epoch();
    {
        let g = lock(conn);
        // OFF by default. The operator's verdict on pushed summaries was that he never
        // reads them, and an unread daily message is pure cost — it teaches him to
        // ignore the channel that also carries a blocked session. `/digest` still gives
        // it on demand, which is when it is actually wanted.
        let enabled = g
            .query_row("SELECT value FROM paos_config WHERE key='digest_enabled'", [], |r| {
                r.get::<_, String>(0)
            })
            .ok()
            .map(|v| paos_memory::doctor::is_truthy(&v))
            .unwrap_or(false);
        if !enabled {
            return;
        }
        let last: i64 = g
            .query_row("SELECT value FROM meta WHERE key=?1", [KEY], |r| r.get::<_, String>(0))
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        if now - last < DAY {
            return;
        }
    }
    // Group by kind, and carry the historical approval rate for that kind. A count alone
    // does not tell him whether it is worth opening; "11 splits, you have approved 85% of
    // those" does.
    let rows: Vec<(String, i64, i64, i64)> = {
        let g = lock(conn);
        let Ok(mut st) = g.prepare(
            "SELECT p.kind, \
                    SUM(p.status='pending'), \
                    (SELECT COUNT(*) FROM memory_proposals q WHERE q.kind=p.kind AND q.status='approved'), \
                    (SELECT COUNT(*) FROM memory_proposals q WHERE q.kind=p.kind AND q.status='rejected') \
             FROM memory_proposals p GROUP BY p.kind HAVING SUM(p.status='pending') > 0",
        ) else {
            return;
        };
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map(|it| it.filter_map(Result::ok).collect())
            .unwrap_or_default()
    };
    if rows.is_empty() {
        return;
    }
    let total: i64 = rows.iter().map(|(_, p, _, _)| p).sum();
    let mut msg = format!("🗂 {total} memory change(s) waiting on you\n");
    for (kind, pending, ok, no) in &rows {
        let rate = if ok + no > 0 {
            format!(" — you have approved {:.0}% of these before", 100.0 * *ok as f64 / (ok + no) as f64)
        } else {
            String::new()
        };
        msg.push_str(&format!("  {pending} × {kind}{rate}\n"));
    }
    // The nudge now carries the review button itself. Pointing at a dashboard is the
    // whole complaint: this arrives on a phone, and "go to your laptop" is not an action.
    msg.push_str("\nNothing is stored until you approve.");
    let (tid, markup) = {
        let g = lock(conn);
        (topic_for(&g, cfg, "ad-hocs"), crate::tg_digest::markup(&g, false, ""))
    };
    if telegram::send_with_markup(cfg, &msg, true, tid, markup.as_deref()).is_ok() {
        // Only advance on a SUCCESSFUL send. Recording it regardless would swallow a
        // day's digest whenever Telegram happened to be down — the same mistake that
        // burned the mirror cursor and lost four messages.
        let g = lock(conn);
        let _ = g.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
            rusqlite::params![KEY, now.to_string()],
        );
    }
}

/// Read the usage cache and ask whether the machine is out of Claude capacity.
///
/// Reads the FILE rather than shelling out to a helper: this runs on every supervise pass,
/// and the whole point of the accounts port was to stop paying an interpreter boot to read
/// a JSON document the daemon can open itself.
///
/// Every failure here is "cannot tell", never "exhausted". A missing or unparseable cache
/// must not page the operator — the alert is only worth having if it cannot cry wolf, and
/// an absent cache is the normal state on a machine with no accounts configured.
fn accounts_exhaustion() -> Option<String> {
    let body = std::fs::read_to_string(op::accounts::cache_path()).ok()?;
    let accounts = op::accounts::parse(&body)?;
    if accounts.is_empty() {
        return None;
    }
    let cfg = op::accounts::SwitchConfig::default();
    // last_switch_ts = 0: a cooldown must never mask exhaustion. Cooldown means "we just
    // switched", which is precisely when the picker has already been told there is nowhere
    // to go — suppressing the alert for it would delay the page by the cooldown window.
    op::accounts::exhaustion(&accounts, &cfg, 0, now_epoch())
}

/// Rooms with a live session and no topic — the selection, separated from the network
/// call so it can be tested without Telegram.
fn rooms_needing_topics(conn: &Connection) -> Vec<String> {
    rooms_with_readers(conn)
        .into_iter()
        .map(|(r, _)| r)
        .filter(|r| {
            conn.query_row("SELECT 1 FROM tg_topics WHERE kind='room' AND key=?1", [r], |_| Ok(()))
                .is_err()
        })
        .collect()
}

/// Create Telegram topics for rooms that have a live session and no topic yet.
///
/// Deliberately silent about rooms it skips: this runs on a timer, and a line per skipped
/// room every cycle is how a log stops being read. It logs what it CREATES, which is rare
/// and is the thing worth knowing.
fn reconcile_topics(conn: &Arc<Mutex<Connection>>, cfg: &Config) {
    if !cfg.is_group {
        return;
    }
    let rooms = { rooms_needing_topics(&lock(conn)) };
    for room in rooms {
        // One at a time, each taking and releasing the lock: `topic_for` makes a network
        // call, and holding the single writer across it would stall every session's
        // memory write behind Telegram's latency.
        let g = lock(conn);
        if topic_for(&g, cfg, &room).is_some() {
            eprintln!("paosd: room {room} has live readers — gave it a topic");
        }
    }

    // And the other direction: a CLOSED room whose topic is still open. Nothing in this
    // workspace ever closed a topic — `closed_ts` was only cleared, never set — so the
    // group grew one topic per room ever opened and the operator had to read past all of
    // them to find the live ones.
    //
    // The list is COLLECTED and the guard DROPPED before the loop. Writing
    // `for x in stale_topics(&lock(conn))` keeps the temporary guard alive for the whole
    // loop, and the body locks again — a self-deadlock on a std Mutex that wedges the
    // bridge thread silently. The socket thread keeps answering `ping`, so the daemon
    // looks healthy while Telegram has stopped entirely. It did exactly that here, and the
    // symptom was a sweep that logged nothing at all rather than an error.
    let stale = { stale_topics(&lock(conn)) };
    for (room, tid) in stale {
        match telegram::close_topic(cfg, tid) {
            // Record ONLY on success, so a failed call is retried next tick rather than
            // being remembered as done.
            Ok(_) => {
                let _ = lock(conn).execute(
                    "UPDATE tg_topics SET closed_ts=?2 WHERE kind='room' AND key=?1",
                    rusqlite::params![room, now_iso()],
                );
                eprintln!("paosd: room {room} is closed — closed its topic {tid}");
            }
            Err(e) => eprintln!("paosd: could not close topic {tid} for {room}: {e}"),
        }
    }
}

/// Open topics whose room is closed. The inverse of `rooms_needing_topics`.
///
/// `questions` is excluded because it is a pseudo-room: it has no row in `rooms` and
/// exists only to give escalations a topic. Treating "no room row" as "closed" would shut
/// the one topic the operator answers questions in.
fn stale_topics(conn: &Connection) -> Vec<(String, i64)> {
    conn.prepare(
        "SELECT t.key, t.thread_id FROM tg_topics t \
         JOIN rooms r ON r.room = t.key \
         WHERE t.kind='room' AND t.closed_ts IS NULL AND r.closed_ts IS NOT NULL",
    )
    .and_then(|mut s| {
        s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map(|it| it.filter_map(Result::ok).collect())
    })
    .unwrap_or_default()
}

fn supervise_and_alert(
    conn: &Arc<Mutex<Connection>>,
    cfg: &Config,
    last: &mut std::time::Instant,
    alerted: &mut std::collections::HashSet<String>,
) {
    if last.elapsed().as_secs() < SUPERVISE_EVERY_SECS {
        return;
    }
    *last = std::time::Instant::now();

    // Give every room a live session is IN a topic to be reached in.
    //
    // Topics used to appear only as a side effect of mirroring: `topic_for` runs when a
    // message is addressed to the operator, so a room whose sessions only talked to each
    // other stayed invisible in the group. Measured at the moment this was written, four
    // live rooms had no topic — including `qbo-queries-sync` with FOUR live sessions in
    // it, which the operator had no way to type into at all. Their words: "new bus rooms
    // are not created as telegram group topics".
    //
    // Membership, not existence, is the trigger. Creating a topic per row in `rooms`
    // would fill the group with the archaeology of every room ever opened; a room with a
    // live reader is exactly a room the operator might need.
    reconcile_topics(conn, cfg);

    // Liveness must work ACROSS implementations while Python still owns the bus CLI.
    // A Python listener is invisible to this daemon's push registry, so asking only the
    // registry would flag every live Python session as deaf. Both implementations hold
    // the same advisory lock file per handle, so that file is the shared truth — and
    // unlike the process table (which returns nothing inside the agent sandbox) a flock
    // cannot be fooled.
    let _ = {
        let g = lock(conn);
        paos_presence::supervise(&g, now_epoch(), &|name| listener_lock_held(name))
    };

    // JOURNAL DEAFNESS UNCONDITIONALLY — above the push gate, deliberately.
    //
    // This block used to sit BELOW the `may_push` return, so in attended mode (the normal
    // case) a deaf session was flagged in the database and then neither alerted nor
    // recorded. Two sessions were flagged deaf on this machine with zero `session.deaf`
    // events, and the newest such event was written by the Python daemon retired earlier
    // today — the capability had been half-dead since the cutover and nothing said so.
    //
    // It is the same defect as the `if !may_push { continue }` that once disabled the
    // whole outbound loop: local bookkeeping must never depend on the phone channel being
    // open. Recording costs nothing when nobody is listening; NOT recording means the one
    // failure a session cannot report about itself leaves no trace at all.
    let deaf_now: Vec<String> = {
        let g = lock(conn);
        let Ok(mut st) = g.prepare(
            "SELECT name FROM sessions WHERE ended_ts IS NULL AND deaf_since IS NOT NULL")
        else { return };
        let names: Vec<String> = st.query_map([], |r| r.get::<_, String>(0))
            .map(|it| it.filter_map(Result::ok).collect())
            .unwrap_or_default();
        for name in &names {
            // A SEPARATE dedupe key from the Telegram alert. Sharing one would mean that
            // journaling in attended mode consumed the key and the operator never got
            // paged if the channel opened later — and using the alert's key here does not
            // dedupe at all in attended mode, because nothing ever inserts it. First
            // version of this wrote a row every 60s per deaf session, forever.
            if alerted.insert(format!("deaf-journal:{name}")) {
                let _ = g.execute(
                    "INSERT INTO events(ts, kind, session, summary) VALUES(?1,?2,?3,?4)",
                    rusqlite::params![
                        now_iso(), "session.deaf", name,
                        format!("{name} DEAF — in rooms but no listener armed")],
                );
            }
        }
        names
    };
    // Forget the journal key once a session recovers, so a LATER episode is recorded as a
    // new one rather than swallowed by the first.
    {
        let live: std::collections::HashSet<String> =
            deaf_now.iter().map(|n| format!("deaf-journal:{n}")).collect();
        alerted.retain(|k| !k.starts_with("deaf-journal:") || live.contains(k));
    }

    // OUT OF CLAUDE CAPACITY — the failure that stalled the whole fleet in silence.
    //
    // The switcher cannot switch to an account that does not exist. On 2026-08-01 one
    // account was weekly-exhausted and the other two burned their 5-hour windows, so every
    // session simply started failing and NOTHING said why. The operator diagnosed it by
    // noticing the fleet was dead.
    //
    // Journalled unconditionally and alerted through the same path as deafness, for the
    // same reason: local bookkeeping must not depend on the phone channel being open.
    let exhausted = accounts_exhaustion();
    match &exhausted {
        Some(detail) => {
            if alerted.insert("accounts-exhausted".to_string()) {
                {
                    let g = lock(conn);
                    let _ = g.execute(
                        "INSERT INTO events(ts, kind, session, summary) VALUES(?1,?2,?3,?4)",
                        rusqlite::params![now_iso(), "accounts.exhausted", "", detail],
                    );
                }
                // PUSHED REGARDLESS OF MODE — the one alert exempt from `may_push`.
                //
                // Every other push is gated because the operator asked not to be bothered
                // while attended or autonomous, and that is right: those messages are
                // things a session could have handled or that can wait. This one cannot.
                // Autonomous means "proceed on your own judgment within policy"; there is
                // no judgment that proceeds through having no Claude capacity. Every
                // session on the machine is about to fail and none of them can fix it.
                //
                // Gating it would have made this alert useless on 2026-08-01 exactly: the
                // fleet stalled while the operator was away-but-not-in-Away-mode, and the
                // way they found out was that everything died.
                //
                // Safe to exempt only because it cannot spam: it fires once per episode
                // (the dedupe key above) and the key is dropped only on recovery.
                let body = telegram::neutralize_mentions(
                    &format!("\u{26a0} {detail}"), cfg.operator_username.as_deref());
                // Audible: this is the one worth a sound.
                let _ = telegram::send(cfg, &body, true, None);
            }
        }
        // Recovered — drop the key so the NEXT episode alerts instead of being swallowed
        // by the first. A 5-hour window resetting is exactly this transition, and without
        // it the operator would be told once, ever.
        None => { alerted.remove("accounts-exhausted"); }
    }

    // Only push when the channel is open; otherwise it queues for when it is.
    {
        let g = lock(conn);
        if !op::may_push(&g, now_epoch()) {
            return;
        }
    }

    // Account caps are PULL, not push (operator's call): `/panel` and `/accounts` show
    // them on demand, and the dashboard carries them too. Off by default.
    //
    // What that costs, stated plainly rather than buried: a weekly cap stops every
    // session at once, and you will now find out by a session failing rather than by a
    // warning. `paos config set account_alerts 1` restores the notice.
    let account_alerts = {
        let g = lock(conn);
        g.query_row("SELECT value FROM paos_config WHERE key='account_alerts'", [], |r| {
            r.get::<_, String>(0)
        })
        .ok()
        .map(|v| paos_memory::doctor::is_truthy(&v))
        .unwrap_or(false)
    };
    if let Some(list) = op::accounts::snapshot_local().filter(|_| account_alerts) {
        for a in list.iter().filter(|a| a.is_critical()) {
            if alerted.insert(format!("acct:{}", a.slot)) {
                let tid = { let g = lock(conn); topic_for(&g, cfg, "ad-hocs") };
                let _ = telegram::send(cfg, &format!(
                    "🔴 Claude account {} has hit {:.0}% of its weekly limit.\n\
                     /switch rotates to the account with the most headroom.",
                    a.slot, a.shown_7d()), false, tid);
            }
        }
        // Re-arm once it recovers, so the next crossing is announced again.
        let critical: std::collections::HashSet<String> =
            list.iter().filter(|a| a.is_critical())
                .map(|a| format!("acct:{}", a.slot)).collect();
        alerted.retain(|k| !k.starts_with("acct:") || critical.contains(k));
    }

    // Doctor findings. Everything doctor detects is a SILENT failure — that is why it
    // exists — so a report nobody reads is worth very little. Five such failures shipped
    // undetected on this machine before doctor existed; each was found by accident.
    //
    // Only FAIL is pushed. WARN is for states that are decisions or setup (dream
    // switched off, Telegram unconfigured), and paging the operator about a choice he
    // made is how an alert channel gets muted.
    {
        let findings: Vec<(String, String, bool)> = {
            let g = lock(conn);
            paos_memory::doctor::run(&g)
                .into_iter()
                .filter(|c| c.level == paos_memory::doctor::Level::Fail)
                .map(|c| {
                    let urgent = c.is_urgent();
                    (c.name.to_string(),
                     format!("{}{}", c.detail,
                             c.fix.map(|f| format!("\n{f}")).unwrap_or_default()),
                     urgent)
                })
                .collect()
        };
        for (name, detail, urgent) in &findings {
            if alerted.insert(format!("doctor:{name}")) {
                let tid = { let g = lock(conn); topic_for(&g, cfg, "ad-hocs") };
                // Silent for anything that is not blocking work: still on the phone,
                // still in the log, no sound at 3am for a missed nightly job.
                let _ = telegram::send(cfg, &format!("🩺 paos: {name} — {detail}"),
                                       !*urgent, tid);
            }
        }
        // Re-arm on recovery, so the NEXT time it breaks you hear about it. Without this
        // a transient failure permanently silences that check.
        let failing: std::collections::HashSet<String> =
            findings.iter().map(|(n, _, _)| format!("doctor:{n}")).collect();
        alerted.retain(|k| !k.starts_with("doctor:") || failing.contains(k));
    }

    // Once a day, if anything is waiting. The review queue only works if the operator
    // knows it has contents; `curate` died because 44 unread proposals turned into 44
    // rejections and then into a habit of not looking. A digest is deliberately chosen
    // over auto-applying: the track record is not uniform — split proposals run ~85%
    // approved and the stale-fact audit 9 of 9, but dream captures only ~15%, so a blanket
    // "trust the machine" rule would quietly write the class he rejects most.
    // A deaf session is invisible by construction — say so unprompted.
    let deaf: Vec<String> = {
        let g = lock(conn);
        let mut st = match g.prepare(
            "SELECT name FROM sessions WHERE ended_ts IS NULL AND deaf_since IS NOT NULL") {
            Ok(s) => s,
            Err(_) => return,
        };
        st.query_map([], |r| r.get::<_, String>(0))
            .map(|it| it.filter_map(Result::ok).collect())
            .unwrap_or_default()
    };
    for name in &deaf {
        if alerted.insert(format!("deaf:{name}")) {
            // The row was already written above the gate; this is only the page.
            let tid = { let g = lock(conn); topic_for(&g, cfg, "ad-hocs") };
            let _ = telegram::send(cfg, &format!(
                "⚠ session {name} is DEAF — it is in rooms but nothing is listening, so \
                 messages addressed to it are being ignored."), false, tid);
        }
    }
    let live: std::collections::HashSet<String> =
        deaf.iter().map(|n| format!("deaf:{n}")).collect();
    alerted.retain(|k| !k.starts_with("deaf:") || live.contains(k));
}

/// Deliver queued `paos operator say` messages.
///
/// This table was never drained by the Rust bridge, so every `say` a session made
/// vanished into SQLite with no error — a silent black hole on the one path a session
/// has to volunteer information to the human.
fn drain_outbox(conn: &Arc<Mutex<Connection>>, cfg: &Config) {
    let rows = { let g = lock(conn); op::unsent_outbox(&g).unwrap_or_default() };
    for (id, session, text) in rows {
        let body = telegram::neutralize_mentions(
            &format!("{text}\n\n— {session}"), cfg.operator_username.as_deref());
        let tid = { let g = lock(conn); topic_for(&g, cfg, "ad-hocs") };
        match telegram::send(cfg, &body, false, tid) {
            Ok(mid) => {
                let g = lock(conn);
                let _ = op::mark_outbox_sent(&g, id, &now_iso(), mid);
                if let Some(m) = mid {
                    let _ = op::record_tg_message(&g, m, &session, &now_iso());
                }
            }
            Err(e) => {
                eprintln!("paosd: outbox: {e}");
                break;   // retry next pass rather than dropping
            }
        }
    }
}

/// Copy new bus messages to Telegram.
///
/// Silent for fleet chatter — that is the point of a mirror — but a message ADDRESSED
/// TO THE OPERATOR notifies. A silent reply reads as no reply: the operator once asked
/// "anyone here?", was answered in 30 s, and reported receiving nothing.
///
/// The cursor advances only past messages that actually sent. Advancing on a failed
/// send is how four replies were marked delivered and silently lost.
fn mirror(conn: &Arc<Mutex<Connection>>, cfg: &Config, limiter: &mut Limiter) {
    let allowance = limiter.allowance();
    if allowance <= 0 {
        return;
    }
    let (last, rows) = {
        let g = lock(conn);
        let last: i64 = g
            .query_row("SELECT value FROM operator_meta WHERE key='tg_mirror_last_msg_id'", [], |r| {
                r.get::<_, String>(0)
            })
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(-1);
        if last < 0 {
            // FIRST RUN: start from now. A zero cursor means "replay the entire bus into
            // the operator's phone" — measured at 3,021 backlogged messages once.
            let max: i64 = g
                .query_row("SELECT COALESCE(MAX(id),0) FROM messages", [], |r| r.get(0))
                .unwrap_or(0);
            let _ = g.execute(
                "INSERT OR REPLACE INTO operator_meta(key,value) VALUES('tg_mirror_last_msg_id',?1)",
                [max.to_string()],
            );
            return;
        }
        let mut stmt = match g.prepare(
            "SELECT id, room, sender, target, text FROM messages WHERE id > ?1 \
             ORDER BY id LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        let rows: Vec<(i64, String, String, String, String)> = stmt
            .query_map(rusqlite::params![last, allowance], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .map(|it| it.filter_map(Result::ok).collect())
            .unwrap_or_default();
        (last, rows)
    };

    limiter.spend(rows.len() as i64);
    let mut sent_upto = last;
    for (id, room, sender, target, text) in rows {
        if sender == "operator" {
            sent_upto = id; // never echo our own broadcasts back
            continue;
        }
        // ONLY what is addressed to him. Telegram is the channel for reaching the OPERATOR,
        // not a mirror of the bus: peer-to-peer traffic is what the dashboard and `paos bus`
        // exist to show, and it does not belong on his phone.
        //
        // Measured on 2026-07-31, which is why this is now an extracted, tested function
        // rather than an inline expression: in one 20-minute window `lucky-heron` sent 9
        // messages, ZERO of them addressed to the operator, and 8 were pushed to Telegram.
        if !targets_operator(&target) {
            sent_upto = id;
            continue;
        }
        // Route into the room's own topic. Without this every room's traffic piles
        // into the group's General, which is precisely what the topics exist to avoid.
        let tid = { let g = lock(conn); topic_for(&g, cfg, &room) };
        // Neutralise on the way OUT only: the bus keeps real handles, Telegram gets
        // readable ones — and @operator becomes a mention that actually pings.
        let body = telegram::neutralize_mentions(
            &format!("{sender} → {target}\n{text}"),
            cfg.operator_username.as_deref(),
        );
        // Always audible: by construction this is now only messages meant for him.
        match telegram::send(cfg, &body, false, tid) {
            Ok(mid) => {
                // Map the message to its sender so replying to it steers that session.
                if let (Some(m), true) = (mid, sender != "operator") {
                    let g = lock(conn);
                    let _ = op::record_tg_message(&g, m, &sender, &now_iso());
                }
                sent_upto = id;
            }
            Err(e) => {
                // Hold the cursor and retry — but BOUNDED. Without a cap, one message
                // Telegram will never accept is retried every 2s forever and ALL
                // delivery stops. Dropping it silently would be worse, so give up loudly.
                if limiter.stuck_id == id {
                    limiter.stuck_n += 1;
                } else {
                    limiter.stuck_id = id;
                    limiter.stuck_n = 1;
                }
                if limiter.stuck_n > MAX_SEND_ATTEMPTS {
                    eprintln!("paosd: mirror: DROPPING #{id} after {} attempts: {e}", limiter.stuck_n);
                    limiter.stuck_id = 0;
                    limiter.stuck_n = 0;
                    sent_upto = id;
                    continue;
                }
                eprintln!("paosd: mirror: {e}");
                break;
            }
        }
    }
    if sent_upto != last {
        let g = lock(conn);
        let _ = g.execute(
            "INSERT OR REPLACE INTO operator_meta(key,value) VALUES('tg_mirror_last_msg_id',?1)",
            [sent_upto.to_string()],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cheap deterministic embedder — these tests never assert on vectors, only that
    /// the callback path does the right thing to the queue.
    fn emb() -> paos_memory::HashEmbedder {
        paos_memory::HashEmbedder::new(64)
    }

    fn db() -> Connection {
        paos_store::open_in_memory().unwrap()
    }

    /// A config that sends nowhere: these tests exercise routing and DB effects, not IO.
    fn cfg() -> Config {
        Config {
            token: String::new(),
            chat_id: String::new(),
            is_group: true,
            operator_username: Some("example_operator".into()),
            allowed_user_id: Some(1),
        }
    }

    /// A live session in a room, and optionally a topic for that room.
    fn seed_room(c: &Connection, room: &str, session: &str, live: bool, topic: Option<i64>) {
        c.execute(
            "INSERT OR IGNORE INTO sessions(name, session_id, ended_ts) VALUES(?1, ?1, ?2)",
            rusqlite::params![session, if live { None } else { Some("2026-08-01T00:00:00Z") }],
        )
        .unwrap();
        c.execute(
            "INSERT OR IGNORE INTO members(room, name, joined_ts, last_seen) \
             VALUES(?1, ?2, '2026-08-01T00:00:00Z', '2026-08-01T00:00:00Z')",
            rusqlite::params![room, session],
        )
        .unwrap();
        if let Some(t) = topic {
            c.execute(
                "INSERT OR IGNORE INTO tg_topics(kind, key, thread_id, created_ts) \
                 VALUES('room', ?1, ?2, '2026-08-01T00:00:00Z')",
                rusqlite::params![room, t],
            )
            .unwrap();
        }
    }

    #[test]
    fn a_room_with_a_live_session_and_no_topic_is_nominated() {
        // THE reported bug. Topics only ever appeared as a side effect of mirroring a
        // message to the operator, so a room whose sessions only talked to each other was
        // invisible in the group — measured live, `qbo-queries-sync` had four live
        // sessions and no topic, and the operator could not type into it at all.
        let c = db();
        seed_room(&c, "qbo-queries-sync", "swift-otter", true, None);
        assert_eq!(rooms_needing_topics(&c), vec!["qbo-queries-sync".to_string()]);
    }

    #[test]
    fn a_closed_rooms_topic_is_nominated_for_closing() {
        // Nothing in this workspace ever closed a topic, so the group grew one per room
        // ever opened. Four were live for closed rooms when this was written.
        let c = db();
        seed_room(&c, "motion-fleet", "old-badger", false, Some(164));
        // The `rooms` row explicitly: `seed_room` seeds sessions, members and topics, and
        // an UPDATE against a row that does not exist affects nothing and asserts nothing.
        c.execute(
            "INSERT INTO rooms(room, created_ts, closed_ts) \
             VALUES('motion-fleet','2026-07-01T00:00:00Z','2026-08-05T00:00:00Z')",
            [],
        )
        .unwrap();
        assert_eq!(stale_topics(&c), vec![("motion-fleet".to_string(), 164)]);
    }

    #[test]
    fn the_escalation_topic_is_never_closed_by_the_sweep() {
        // `questions` is a PSEUDO-room: it has no row in `rooms` at all. Treating a
        // missing room row as "closed" would shut the one topic the operator answers
        // questions in — the exact opposite of the point.
        let c = db();
        c.execute("INSERT INTO tg_topics(kind, key, thread_id, created_ts) \
                   VALUES('room','questions',498,'2026-08-01T00:00:00Z')", []).unwrap();
        assert!(stale_topics(&c).is_empty());
    }

    #[test]
    fn an_open_rooms_topic_is_left_alone() {
        let c = db();
        seed_room(&c, "ad-hocs", "swift-otter", true, Some(195));
        c.execute("INSERT INTO rooms(room, created_ts) VALUES('ad-hocs','2026-07-01T00:00:00Z')",
                  []).unwrap();
        assert!(stale_topics(&c).is_empty());
    }

    #[test]
    fn a_room_that_already_has_a_topic_is_not_nominated_again() {
        // Nominating it twice would create a SECOND Telegram topic with the same name and
        // orphan the first — the failure that once left three "ad-hocs" topics in the
        // group, undiscoverable because bots cannot list topics.
        let c = db();
        seed_room(&c, "ad-hocs", "swift-otter", true, Some(195));
        assert!(rooms_needing_topics(&c).is_empty());
    }

    #[test]
    fn a_room_whose_sessions_have_all_ended_is_not_nominated() {
        // Membership outlives the session — the reap fires when the session ROW goes, and
        // ending one only sets `ended_ts`. Without the liveness join this would create a
        // topic for every room ever used, which is the archaeology of the whole fleet.
        let c = db();
        seed_room(&c, "motion-fleet", "old-badger", false, None);
        assert!(rooms_needing_topics(&c).is_empty());
    }

    fn last_msg(c: &Connection) -> (String, String, String, i64) {
        c.query_row(
            "SELECT room, target, text, COALESCE(ambient,0) FROM messages ORDER BY id DESC LIMIT 1",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        ).unwrap()
    }

    #[test]
    fn a_message_typed_into_an_empty_room_tells_the_operator_it_reached_nobody() {
        // `post_as_operator` INSERT-OR-IGNOREs the room, so anything typed into a topic
        // whose room has no members is stored where nobody is — silently, forever.
        let live = [("agentic-brain-e2e".to_string(), 6), ("ad-hocs".to_string(), 3)];
        let w = unread_room_warning("some-dead-room", 0, &live).expect("must warn");
        assert!(w.contains("NOBODY RECEIVED THAT"), "the consequence must lead: {w}");
        assert!(w.contains("some-dead-room"), "must name the dead room: {w}");
        // Naming a dead room is only half a message — it must say where to type INSTEAD,
        // or the operator knows they failed and not what to do about it.
        assert!(w.contains("agentic-brain-e2e"), "must offer a room that works: {w}");

        // A room with readers is silent. This is the assertion that keeps the warning worth
        // reading: it must not fire on the normal case.
        assert!(unread_room_warning("ad-hocs", 3, &live).is_none());

        // Fleet entirely down: do not print an empty list and imply there is somewhere to go.
        let w = unread_room_warning("some-dead-room", 0, &[]).expect("must still warn");
        assert!(w.contains("No room currently has"), "must not offer an empty list: {w}");
    }

    #[test]
    fn a_topic_title_names_the_project_it_belongs_to() {
        // The operator picks a topic to open from this string, on a phone. Whether it says
        // which project it is was previously an accident of what the room had declared on
        // the day the topic happened to be created — `# wave2 · agentic-brain` did,
        // `# ad-hocs` did not, and both rooms declare a repo.
        assert_eq!(topic_title("ad-hocs", Some("dotfiles")), "# ad-hocs · dotfiles");
        assert_eq!(topic_title("motion-fleet", Some("motion,motion-client-dashboard-ops")),
                   "# motion-fleet · motion,motion-client-dashboard-ops");
        // lobby is the directory and is about no repo — no dangling separator.
        assert_eq!(topic_title("lobby", None), "# lobby");
        assert_eq!(topic_title("lobby", Some("")), "# lobby", "empty is not a repo list");
        assert_eq!(topic_title("lobby", Some("   ")), "# lobby", "nor is whitespace");
        // `questions` is not a room; a `# ` title would present the escalation channel as
        // if it were a project.
        assert_eq!(topic_title("questions", Some("anything")), "❓ questions · reply to answer");
    }

    #[test]
    fn the_desired_title_follows_the_rooms_repos() {
        // End to end against the db, because the bug being fixed is that the title and the
        // room's repos drift apart — a pure-function test alone would not show that.
        let mut c = db();
        c.execute("INSERT INTO rooms(room, created_ts, repos) VALUES('r','t','dotfiles')", [])
            .unwrap();
        assert_eq!(desired_title(&c, "r"), "# r · dotfiles");
        c.execute("UPDATE rooms SET repos='dotfiles,paos' WHERE room='r'", []).unwrap();
        assert_eq!(desired_title(&c, "r"), "# r · dotfiles,paos", "must follow a change");
        c.execute("UPDATE rooms SET repos=NULL WHERE room='r'", []).unwrap();
        assert_eq!(desired_title(&c, "r"), "# r");
        // A room with no row at all must not panic or invent a suffix.
        assert_eq!(desired_title(&c, "never-created"), "# never-created");
    }

    #[test]
    fn typing_in_the_escalation_topic_says_to_quote_reply_instead() {
        // `questions` is not a room — it is the pseudo-room this file uses to give every
        // escalation ONE Telegram topic (see the escalation loop). I initially read it as
        // an orphan and that inverted the severity: it is the topic the operator is MOST
        // likely to type an answer into, and plain text there reaches nobody.
        //
        // The generic advice is actively wrong here. "Type in one of these other topics"
        // does not answer a question; the room they need belongs to the asking session and
        // only the escalation message knows which. So: quote-reply.
        let live = [("ad-hocs".to_string(), 3)];
        let w = unread_room_warning("questions", 0, &live).expect("must warn");
        assert!(w.contains("REPLY to the ❓ message"), "must give the path that works: {w}");
        assert!(!w.contains("ad-hocs"),
                "must NOT offer other rooms — none of them answers the question: {w}");
    }

    #[test]
    fn the_empty_room_check_counts_only_live_sessions() {
        // REBUILT FROM THE DATABASE AFTER MY FIRST VERSION WAS WRONG. I had asserted
        // liveness on a bare `members` count, on the strength of a comment in paos-presence
        // saying membership is reaped at session end. It is not: the reap keys on the
        // SESSION ROW vanishing, and ending a session sets `ended_ts` and keeps the row.
        //
        // The shape below is the real one, from the live db: motion-fleet had 3 members
        // and 0 live sessions. Under the old query it read as a healthy room.
        let mut c = db();
        assert!(rooms_with_readers(&c).is_empty(), "a fresh db has no readers");
        let add = |name: &str, room: &str, ended: Option<&str>| {
            c.execute("INSERT OR IGNORE INTO sessions(name, started_ts, ended_ts) VALUES(?1,'t',?2)",
                      rusqlite::params![name, ended]).unwrap();
            c.execute("INSERT INTO members(room, name, last_seen) VALUES(?1,?2,'t')",
                      rusqlite::params![room, name]).unwrap();
        };
        add("live-a", "busy", None);
        add("live-b", "busy", None);
        add("live-a", "quiet", None);
        // motion-fleet: members present, every one of them ended.
        add("gone-1", "motion-fleet", Some("2026-07-31T14:41:57Z"));
        add("gone-2", "motion-fleet", Some("2026-07-31T09:31:31Z"));
        add("gone-3", "motion-fleet", Some("2026-07-31T09:31:37Z"));

        let got = rooms_with_readers(&c);
        assert!(!got.iter().any(|(r, _)| r == "motion-fleet"),
                "a room whose sessions have all ended must not count as reachable: {got:?}");
        assert_eq!(got, vec![("busy".to_string(), 2), ("quiet".to_string(), 1)],
                   "most-populated first, so the suggestion leads with the likeliest room");

        // A member row that outlives its session must not resurrect the room.
        c.execute("UPDATE sessions SET ended_ts='2026-08-01T00:00:00Z' WHERE name='live-b'", [])
            .unwrap();
        assert_eq!(rooms_with_readers(&c),
                   vec![("busy".to_string(), 1), ("quiet".to_string(), 1)]);
    }

    #[test]
    fn plain_text_into_a_room_with_readers_posts_and_stays_quiet() {
        // End to end through handle_message: the post must still happen. The warning is
        // ADDITIVE — an operator message into a dead room is still stored and still
        // readable via `paos bus log`, so refusing to post would destroy information.
        let mut c = db();
        c.execute("INSERT INTO members(room, name, last_seen) VALUES('lobby','s1','t')", []).unwrap();
        handle_message(&mut c, &cfg(), "hello fleet", None, None, None);
        let (room, target, text, _) = last_msg(&c);
        assert_eq!((room.as_str(), target.as_str()), ("lobby", "@all"));
        assert!(text.contains("hello fleet"));
    }

    #[test]
    fn mode_commands_set_the_mode_and_broadcast_ambiently() {
        let mut c = db();
        handle_message(&mut c, &cfg(), "/away", None, None, None);
        assert_eq!(op::get_mode(&c), op::Mode::Away);
        let (room, _, text, ambient) = last_msg(&c);
        assert_eq!(room, "lobby");
        assert_eq!(ambient, 1, "a mode banner must not wake the fleet");
        assert!(text.contains("operator mode"), "{text}");
    }

    #[test]
    fn plain_text_lands_in_the_room_whose_topic_it_was_typed_in() {
        // REGRESSION: this used to hardcode ad-hocs, so typing in a room's topic posted
        // somewhere else entirely — to a room most sessions are not in.
        let mut c = db();
        c.execute("INSERT INTO tg_topics(kind,key,thread_id,created_ts) \
                   VALUES('room','motion-fleet',164,'t')", []).unwrap();
        handle_message(&mut c, &cfg(), "status please", Some(164), None, None);
        let (room, target, text, _) = last_msg(&c);
        assert_eq!(room, "motion-fleet");
        assert_eq!(target, "@all");
        assert!(text.contains("📱 operator:"), "{text}");
    }

    #[test]
    fn a_message_outside_any_room_topic_broadcasts_to_the_fleet() {
        // Changed from ad-hocs on operator request: a message with no topic means
        // "everyone", and lobby is the room every session joins. A message typed INSIDE
        // a room's topic still goes to that room.
        let mut c = db();
        handle_message(&mut c, &cfg(), "hello", Some(9999), None, None);
        assert_eq!(last_msg(&c).0, "lobby");
    }

    #[test]
    fn a_message_in_a_room_topic_goes_to_that_room() {
        let mut c = db();
        c.execute("INSERT INTO tg_topics(kind,key,thread_id,title,created_ts) \
                   VALUES('room','ad-hocs',77,'ad-hocs','t')", []).unwrap();
        handle_message(&mut c, &cfg(), "hello", Some(77), None, None);
        assert_eq!(last_msg(&c).0, "ad-hocs");
    }

    #[test]
    fn at_handle_resolves_a_short_name_to_a_live_session() {
        // The operator types @memphis, not @rustic-otter-2.
        let mut c = db();
        c.execute("INSERT INTO sessions(name,updated_ts) VALUES('swift-otter-memphis','t')", []).unwrap();
        handle_message(&mut c, &cfg(), "@memphis please rebase", None, None, None);
        let (room, target, text, _) = last_msg(&c);
        assert_eq!(room, "lobby");
        assert_eq!(target, "@swift-otter-memphis", "must resolve, not pass through verbatim");
        assert!(text.contains("please rebase"));
    }

    #[test]
    fn an_unresolvable_handle_posts_nothing_to_the_bus() {
        // Passing it through verbatim addressed nobody, silently.
        let mut c = db();
        handle_message(&mut c, &cfg(), "@nosuchsession hi", None, None, None);
        let n: i64 = c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn a_quote_reply_to_an_escalation_answers_it() {
        // THE blocking gap: escalations were unanswerable anywhere in the system.
        let mut c = db();
        let id = op::ask(&c, "swift-otter", "deploy?", None, "t").unwrap();
        op::set_escalation_message_id(&c, id, 555).unwrap();
        handle_message(&mut c, &cfg(), "yes ship it", None, Some(555), None);
        let (status, answer): (String, Option<String>) = c
            .query_row("SELECT status, answer FROM escalations WHERE id=?1", [id],
                       |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(status, "answered");
        assert_eq!(answer.as_deref(), Some("yes ship it"));
    }

    #[test]
    fn a_quote_reply_to_a_session_message_steers_that_session() {
        let mut c = db();
        op::record_tg_message(&c, 777, "quiet-bison", "t").unwrap();
        handle_message(&mut c, &cfg(), "hold off", None, Some(777), None);
        let (room, target, _, _) = last_msg(&c);
        assert_eq!(room, "lobby");
        assert_eq!(target, "@quiet-bison");
    }

    #[test]
    fn tapping_an_option_button_answers_the_escalation() {
        let mut c = db();
        let id = op::ask(&c, "s", "ship or hold?", Some("ship now,hold"), "t").unwrap();
        handle_callback(&mut c, &cfg(), &format!("esc:{id}:1"), None, None, None, &emb());
        let answer: Option<String> = c
            .query_row("SELECT answer FROM escalations WHERE id=?1", [id], |r| r.get(0)).unwrap();
        assert_eq!(answer.as_deref(), Some("hold"));
    }

    #[test]
    fn a_malformed_callback_is_ignored_not_a_panic() {
        let mut c = db();
        for data in ["", "nonsense", "esc:notanumber:0", "esc:1", "esc:1:99"] {
            handle_callback(&mut c, &cfg(), data, None, None, None, &emb());
        }
    }

    #[test]
    fn empty_input_posts_nothing() {
        let mut c = db();
        for t in ["", "   ", "\n"] {
            handle_message(&mut c, &cfg(), t, None, None, None);
        }
        let n: i64 = c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn commands_do_not_leak_onto_the_bus_as_operator_chatter() {
        // /digest and friends used to be unrecognised and posted verbatim to the bus.
        let mut c = db();
        for cmd in ["/digest", "/who", "/blocked", "/parked", "/help", "/tasks",
                    // The @botname form Telegram actually sends in a group. Before this
                    // was handled these fell through and were broadcast to the fleet as
                    // operator chatter, so the command both failed and made noise.
                    "/tasks@example_bot", "/who@example_bot", "/digest@example_bot"] {
            handle_message(&mut c, &cfg(), cmd, None, None, None);
        }
        let n: i64 = c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "commands must be answered, not broadcast");
    }

    #[test]
    fn digest_reports_what_needs_the_operator() {
        let mut c = db();
        assert!(op::digest(&c).contains("all quiet"));
        op::ask(&c, "swift-otter", "deploy to prod?", None, "t").unwrap();
        let d = op::digest(&c);
        assert!(d.contains("deploy to prod?") && d.contains("swift-otter"), "{d}");
    }

    #[test]
    fn operator_posts_are_attributed_to_the_operator_identity() {
        let mut c = db();
        post_as_operator(&c, "lobby", "@all", "hello", false).unwrap();
        let s: String = c
            .query_row("SELECT sender FROM messages ORDER BY id DESC LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(s, "operator");
    }

    #[test]
    fn the_rate_limiter_caps_a_burst_and_then_refills() {
        // 450 msg/min against Telegram's ~20/min cap guarantees 429 storms.
        let mut l = Limiter::default();
        let first = l.allowance();
        assert!(first <= MIRROR_BATCH);
        l.spend(first);
        assert_eq!(l.allowance(), 0, "budget must actually be spent");
        l.last = Some(std::time::Instant::now() - std::time::Duration::from_secs(120));
        assert!(l.allowance() > 0, "and refill over time");
    }

    #[test]
    fn only_operator_addressed_messages_reach_telegram() {
        // Every string here is a REAL target taken from the messages table on 2026-07-31.
        // In one 20-minute window lucky-heron sent 9 messages, none addressed to the
        // operator, and 8 were pushed to his phone — which is the whole complaint.
        for peer in ["@vivid-cobra-2", "@cosmic-quokka-3", "@plucky-marten-3",
                     "@zesty-civet-2", "@frosty-shrike", "@witty-bison-2"] {
            assert!(!targets_operator(peer), "{peer} is a PEER — must not reach Telegram");
        }
        // @all is the most common target on the bus. If a broadcast counted as
        // operator-addressed, the phone would receive essentially the entire bus.
        assert!(!targets_operator("@all"), "a broadcast is not a message to the operator");
        assert!(!targets_operator(""), "an empty target is not the operator");
    }

    #[test]
    fn a_message_actually_addressed_to_the_operator_does_reach_telegram() {
        // The other half: over-filtering would silently cut the human off, which is worse
        // than the noise — a session asking a question would get no answer and no error.
        assert!(targets_operator("@operator"));
        assert!(targets_operator("operator"));
        assert!(targets_operator("  @operator  "));
        assert!(targets_operator("@Operator"), "case must not decide reachability");
        // Multi-target: the operator is one of several recipients.
        assert!(targets_operator("@witty-bison-2__operator"));
        assert!(targets_operator("@operator__jolly-dingo-2"));
    }

    #[test]
    fn a_handle_that_merely_contains_operator_is_not_the_operator() {
        // A substring test would route these to the phone. Named because the obvious
        // implementation is the wrong one.
        for near in ["@operator-relay", "@my-operator", "@operators", "@co-operator"] {
            assert!(!targets_operator(near), "{near} is a session handle, not the human");
        }
    }

    #[test]
    fn a_phone_keyboard_mode_label_sets_the_mode() {
        // Observed live on 2026-07-31: the operator's phone sent these as plain TEXT.
        // They are not panel button labels here, so both fell through to the broadcast —
        // the mode did NOT change, and every listening session in lobby woke for it.
        assert_eq!(mode_from_label("✈️ Away"), Some(op::Mode::Away));
        assert_eq!(mode_from_label("🤖 Auto"), Some(op::Mode::Autonomous));
        assert_eq!(mode_from_label("away"), Some(op::Mode::Away));
        assert_eq!(mode_from_label("  Attended  "), Some(op::Mode::Attended));
        assert_eq!(mode_from_label("away!"), Some(op::Mode::Away));
    }

    #[test]
    fn a_sentence_containing_a_mode_word_is_still_a_message() {
        // The boundary matters more than the fix: a false positive eats a message meant
        // for the fleet AND changes the mode behind the operator's back.
        for msg in ["away for lunch, back at 3", "I am away",
                    "auto-switch the account please", "is the daemon here?",
                    "away away", "status?", ""] {
            assert_eq!(mode_from_label(msg), None, "{msg:?} is a message, not a mode");
        }
    }

    #[test]
    fn splitting_before_stripping_is_what_stops_words_fusing() {
        // THIS TEST REPLACES A VACUOUS ONE. The first version asserted "away for lunch"
        // is not a mode — true, but true under BOTH implementations, because stripping
        // first fuses it to "awayforlunch", which matches nothing either way. It passed
        // when I deliberately broke the code, which is the only reason I noticed.
        //
        // A discriminating case needs the fused form to BE a mode word while the split
        // form is not. Strip-first reads "aw ay" as "away" and would silently set Away
        // from a message that says no such thing.
        assert_eq!(mode_from_label("aw ay"), None, "fused text must not become a mode");
        assert_eq!(mode_from_label("he re"), None);
        assert_eq!(mode_from_label("aut o"), None);
    }
    /// Telegram appends `@botname` to commands sent in a group — which is where these are
    /// typed. Unstripped, the command matched nothing, fell through to the chatter path,
    /// and was BROADCAST TO THE FLEET as an operator message: it both failed and made
    /// noise. Observed live with `/tasks@example_bot`.
    #[test]
    fn a_command_with_the_botname_suffix_is_still_a_command() {
        let mut c = db();
        handle_message(&mut c, &cfg(), "/tasks@example_bot", None, None, None);
        handle_message(&mut c, &cfg(), "/who@some_bot", None, None, None);
        let n: i64 = c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "a suffixed command must not leak onto the bus as chatter");
    }

    /// The third instance of one failure: a phone keyboard label arrives as plain text,
    /// matches nothing, and is broadcast to the fleet instead of doing what it says.
    /// `mode_from_label` fixed it for ✈️ Away on 2026-07-31; 👥 Fleet still leaked.
    #[test]
    fn a_phone_keyboard_label_becomes_its_command() {
        assert_eq!(command_from_label("👥 Fleet").as_deref(), Some("/who"));
        assert_eq!(command_from_label("📊 Digest").as_deref(), Some("/digest"));
        assert_eq!(command_from_label("🗂 tasks").as_deref(), Some("/tasks"));
        assert_eq!(command_from_label("🤖 accounts").as_deref(), Some("/accounts"));
        assert_eq!(command_from_label("📋 needs me").as_deref(), Some("/digest"));
        assert_eq!(command_from_label("✈️ Away").as_deref(), Some("/away"));
    }

    /// The point of deriving from BOT_COMMANDS rather than a label list: every command
    /// works as a label, including ones added after this was written. The hand-written
    /// list took three rounds and still missed "📊 Digest".
    #[test]
    fn every_command_is_reachable_as_a_keyboard_label() {
        for (c, _) in BOT_COMMANDS {
            let label = format!("📌 {c}");
            assert_eq!(command_from_label(&label).as_deref(), Some(format!("/{c}").as_str()),
                       "{c} is not reachable from a keyboard label");
        }
    }

    #[test]
    fn the_fleet_list_fits_on_a_phone() {
        // A real status from this machine: ~1,800 characters of handover notes. Twenty of
        // these is three back-to-back 3,900-char Telegram messages — nothing errors, he
        // just gets a wall where he asked "who is up?".
        let mut c = db();
        let long = "PR#63 REBASED ONTO main and the two QBO gateway env vars are SET ON \
                    PRODUCTION web+worker WITHOUT a redeploy, verified file-by-file "
            .repeat(12);
        c.execute("INSERT INTO sessions(name, status, last_seen) VALUES('quiet-bison', ?1, 't')",
                  [&long]).unwrap();
        c.execute("INSERT INTO sessions(name, status, last_seen) VALUES('swift-cobra', '', 't')",
                  []).unwrap();
        let out = fleet(&c);
        assert!(out.lines().all(|l| l.chars().count() <= 120), "one line per session:\n{out}");
        assert!(out.contains("…"), "a long status must be visibly cut, not silently:\n{out}");
        assert!(out.contains("swift-cobra — (idle)"), "an empty status still reads:\n{out}");
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        // Statuses are full of — and →. Slicing one by byte index panics the bridge.
        assert_eq!(one_line("a—b—c—d", 3), "a—…");
        assert_eq!(one_line("short", 90), "short");
        assert_eq!(one_line("two\nlines", 90), "two lines");
    }

    /// The safety margin, and the reason this matches on the emoji rather than the word:
    /// swallowing a real message is worse than the bug it fixes, because the message never
    /// arrives AND a panel opens that nobody asked for.
    #[test]
    fn a_bare_word_stays_a_message_to_the_fleet() {
        assert_eq!(command_from_label("fleet"), None);
        assert_eq!(command_from_label("tasks"), None);
        assert_eq!(command_from_label("accounts"), None);
        assert_eq!(command_from_label("👥 what is the fleet doing"), None);
        assert_eq!(command_from_label("can you check tasks 🗂"), None);
    }

    #[test]
    fn a_panel_label_does_not_leak_onto_the_bus() {
        let mut c = db();
        handle_message(&mut c, &cfg(), "👥 Fleet", None, None, None);
        handle_message(&mut c, &cfg(), "🗂 tasks", None, None, None);
        handle_message(&mut c, &cfg(), "📊 Digest", None, None, None);
        let n: i64 = c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "a tapped panel button must not wake the whole fleet");
    }

    /// Every command answers the ASKER — enforced on the source, because the failure is
    /// invisible from inside a test: replies go to Telegram, so a wrongly-addressed one
    /// looks identical to a correct one here, and shows up only as the operator saying
    /// "I click panel menu in the bot and it goes to general in the group".
    ///
    /// `handle_message` must therefore never name a chat itself. `reply` and
    /// `reply_with_markup` route to the chat the command came from; the raw senders
    /// address the configured group, which is right for what the daemon ORIGINATES and
    /// wrong for every answer.
    #[test]
    fn no_command_answers_into_the_group_instead_of_the_asker() {
        let src = include_str!("bridge.rs");
        let start = src.find("fn handle_message(").expect("handle_message exists");
        let body = &src[start..];
        let end = body.find("\n}\n").expect("its closing brace");
        let body = &body[..end];
        for raw in ["telegram::send(cfg", "telegram::send_with_markup(cfg"] {
            assert!(!body.contains(raw),
                    "handle_message calls {raw}, which answers into the configured chat \
                     rather than the one that asked — use reply/reply_with_markup");
        }
    }

    /// The always-on keyboard, both directions. "It is missing tasks and smth else" is
    /// what an enumerated keyboard always becomes; generated from the command table it
    /// cannot be, and these two tests are what hold that.
    #[test]
    fn every_command_has_a_key_on_the_always_on_keyboard() {
        let kb = keyboard_markup();
        for (c, _) in BOT_COMMANDS {
            assert!(kb.contains(&format!(" {c}\"")), "{c} has no key: {kb}");
        }
    }

    #[test]
    fn a_squared_letter_emoji_is_decoration_not_a_word() {
        // Unicode says 🅿 is alphabetic; the guard used to require a non-alphabetic
        // non-ASCII char, so this label resolved to nothing and was broadcast instead.
        assert_eq!(command_from_label("🅿 parked").as_deref(), Some("/parked"));
        assert_eq!(command_from_label("parked").as_deref(), None,
                   "bare prose is still a message to the fleet");
        assert_eq!(command_from_label("🅿 nosuchcommand").as_deref(), None,
                   "decoration alone is not enough — the word must name a command");
    }

    #[test]
    fn every_key_resolves_back_to_a_command() {
        // A key whose label the parser does not recognise is worse than a missing one:
        // tapping it broadcasts the label to the fleet as operator chatter.
        for (c, _) in BOT_COMMANDS {
            let label = format!("{} {c}", icon(c));
            assert_eq!(command_from_label(&label).as_deref(), Some(format!("/{c}").as_str()),
                       "key {label:?} does not resolve to /{c}");
        }
    }

    /// The panel was missing half the bot: blocked, parked and health had commands but no
    /// buttons, so from a phone they did not exist.
    #[test]
    fn the_panel_offers_every_view_it_can_render() {
        let mut c = db();
        let rows = panel_rows(&c);
        for what in ["digest", "tasks", "blocked", "parked", "who", "accounts", "health"] {
            assert!(rows.contains(&format!("panel:{what}")), "{what} has no button");
            assert!(view(&c, what).is_some(), "{what} has no renderer");
        }
    }

    #[test]
    fn a_command_typed_as_a_reply_still_runs() {
        // THE BUG THAT MADE HIS COMMANDS "SOMETIMES DO NOTHING". Replying to a session's
        // message and typing /tasks relayed the literal string to that session and
        // returned — the command never ran and nothing said so. In a topic where a
        // session is talking to you, replying is the natural gesture, so this was most
        // of the time.
        let mut c = db();
        c.execute("INSERT INTO sessions(name, session_id, updated_ts) \
                   VALUES('swift-otter','s1','t')", []).unwrap();
        let mid = 4242;
        c.execute("INSERT INTO tg_message_map(message_id, session, created_ts) \
                   VALUES(?1,'swift-otter','t')", [mid]).unwrap();
        handle_message(&mut c, &cfg(), "/tasks", None, Some(mid), None);
        let n: i64 = c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "a command replied to a session must RUN, not be relayed as chatter");
    }

    #[test]
    fn a_real_message_typed_as_a_reply_still_reaches_that_session() {
        // The behaviour worth keeping: replying to a session IS how you steer it.
        let mut c = db();
        c.execute("INSERT INTO sessions(name, session_id, updated_ts) \
                   VALUES('swift-otter','s1','t')", []).unwrap();
        let mid = 4243;
        c.execute("INSERT INTO tg_message_map(message_id, session, created_ts) \
                   VALUES(?1,'swift-otter','t')", [mid]).unwrap();
        handle_message(&mut c, &cfg(), "rebase onto main first", None, Some(mid), None);
        let (target, text): (String, String) = c
            .query_row("SELECT target, text FROM messages ORDER BY id DESC LIMIT 1",
                       [], |r| Ok((r.get(0)?, r.get(1)?))).expect("relayed");
        assert_eq!(target, "@swift-otter");
        assert!(text.contains("rebase onto main"), "{text}");
    }

    #[test]
    fn only_the_installed_binary_may_own_the_bridge() {
        // 2026-08-03: a paosd built in a scratch checkout beat the real daemon to the
        // token lock, so the REAL one disabled its bridge and the operator's commands
        // intermittently did nothing for hours. The lock is first-come; this is not.
        let installed = std::path::Path::new("/usr/local/bin/paosd");
        assert!(may_bridge(&Some(installed.to_path_buf()), &None, installed));
        assert!(!may_bridge(&Some("/tmp/scratch/target/debug/paosd".into()), &None, installed),
                "a scratch build must not race the daemon for the operator's messages");
    }

    #[test]
    fn an_explicit_opt_in_still_works_because_someone_will_mean_it() {
        let installed = std::path::Path::new("/usr/local/bin/paosd");
        assert!(may_bridge(&Some("/tmp/x/paosd".into()), &Some("1".into()), installed));
        assert!(!may_bridge(&Some("/tmp/x/paosd".into()), &Some("0".into()), installed),
                "only an explicit 1 — a stray PAOS_ALLOW_BRIDGE= in a shell profile is \
                 not consent");
    }

    #[test]
    fn not_knowing_what_we_are_refuses_rather_than_risks_it() {
        // A wrongly-silent bridge is fixed by restarting the daemon. A wrongly-active one
        // eats the operator's messages and reports nothing.
        assert!(!may_bridge(&None, &None, std::path::Path::new("/usr/local/bin/paosd")));
    }

    #[test]
    fn every_panel_button_resolves_to_a_view() {
        // The other direction: a button whose callback has no renderer is a dead tap.
        let mut c = db();
        let rows = panel_rows(&c);
        for part in rows.split("panel:").skip(1) {
            let what: String = part.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
            assert!(view(&c, &what).is_some(), "button panel:{what} renders nothing");
        }
    }

    #[test]
    fn the_panel_marks_the_active_mode_and_offers_the_others() {
        let mut c = db();
        op::set_mode(&c, op::Mode::Away, "test", "t").unwrap();
        let rows = panel_rows(&c);
        assert!(rows.contains("• ✈️ away"), "the active mode must be marked: {rows}");
        assert!(rows.contains("mode:here") && rows.contains("mode:auto"));
    }

    #[test]
    fn counts_appear_only_when_there_is_something_to_count() {
        let mut c = db();
        assert!(panel_rows(&c).contains("🗂 tasks\""), "no count when empty");
        c.execute(
            "INSERT INTO tasks(id,title,state,priority,scope,origin,created_by,created_ts,\
             updated_ts) VALUES('t-aaaaaa','x','review',2,'global','operator','operator','t','t')",
            []).unwrap();
        assert!(panel_rows(&c).contains("🗂 tasks 1"), "a waiting task must show as a count");
    }

}
