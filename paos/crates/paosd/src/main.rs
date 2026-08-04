//! `paosd` — the PAOS daemon.
//!
//! Owns the SQLite connection, applies migrations once at startup, and answers CLI
//! requests over a unix socket. Everything the audit found expensive follows from this
//! being the only process that touches the database.
//!
//! Threads and blocking I/O, deliberately — not tokio. At single-user scale (one
//! operator, a handful of sessions) an async runtime buys nothing and costs binary
//! size, compile time and cold start.

use paos_bus::push::Registry;
use paos_proto::{read_frame, write_frame, Request};
use paos_store as store;
use std::io;
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};

mod bridge;
mod dream;
mod handlers;
mod tg_digest;
mod tg_tasks;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Shared daemon state. The connection sits behind a mutex because there is exactly one
/// writer by design — serialising here is what makes the `SELECT MAX(seq)+1` race the
/// Python implementation carried structurally impossible rather than merely unlikely.
pub struct Daemon {
    pub conn: Arc<Mutex<rusqlite::Connection>>,
    /// Connected listeners. This is what makes delivery a push instead of a poll.
    pub push: Registry,
    /// `PAOS_BUS_ALL_WAKES=1` restores the old "every @all wakes everyone" behaviour.
    pub broadcast_wakes: bool,
    /// Loaded once at startup: the 123 MB model must not be re-read per request.
    pub embedder: Arc<dyn paos_memory::Embedder>,
    /// The second-stage judge, when its model is present.
    ///
    /// OPTIONAL on purpose. It is a separate 133 MB download, so a fresh install, an
    /// offline machine and anyone who declines it must all still get working recall —
    /// just the single-stage recall they had before.
    pub reranker: Option<Arc<dyn paos_memory::Embedder>>,
}

/// The second-stage model, if it has been downloaded.
///
/// Never fatal: every failure here means single-stage recall, which is what paos did
/// yesterday. A daemon that refuses to start because an optional model is missing would
/// turn an optional feature into a required one.
#[cfg(feature = "bert")]
fn load_reranker() -> Option<Arc<dyn paos_memory::Embedder>> {
    let dir = paos_memory::BertEmbedder::default_dir();
    if !dir.join("model.safetensors").exists() {
        return None;
    }
    match paos_memory::BertEmbedder::from_dir(&dir) {
        Ok(b) => Some(Arc::from(Box::new(b) as Box<dyn paos_memory::Embedder>)),
        Err(e) => {
            eprintln!("paosd: reranker unavailable ({e})");
            None
        }
    }
}

#[cfg(not(feature = "bert"))]
fn load_reranker() -> Option<Arc<dyn paos_memory::Embedder>> {
    None
}

fn main() {
    if let Err(e) = run() {
        eprintln!("paosd: {e}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let db = store::db_path();
    let conn = Arc::new(Mutex::new(store::open(&db).map_err(io::Error::other)?));
    eprintln!("paosd v{VERSION} · db={} · schema v{}", db.display(), store::SCHEMA_VERSION);

    // Validate BEFORE binding. Binding first announced "listening", then failed the
    // check and exited — leaving a socket file behind and a window in which a client
    // could connect to a daemon that was already dying.
    let embedder: Arc<dyn paos_memory::Embedder> = Arc::from(paos_memory::best_available());
    eprintln!("paosd: embedder {} ({} dims)", embedder.id(), embedder.dimensions());
    let reranker = load_reranker();
    match &reranker {
        Some(r) => eprintln!("paosd: reranker {} ({} dims)", r.id(), r.dimensions()),
        // Said out loud rather than left silent: a missing second stage is a supported
        // state, but one nobody should discover by wondering why recall did not improve.
        None => eprintln!("paosd: no reranker — recall is single-stage"),
    }
    {
        let g = conn.lock().unwrap_or_else(|p| p.into_inner());
        paos_memory::ensure_schema(&g).map_err(io::Error::other)?;
        // Refuse to serve a store whose vectors came from a different model rather than
        // silently returning cosine numbers computed across unrelated spaces.
        if let Err(e) = paos_memory::check_space(&g, embedder.as_ref()) {
            return Err(io::Error::other(e));
        }
    }

    let sock = store::socket_path();
    // A leftover socket from an unclean shutdown makes bind() fail with "Address already
    // in use" forever. Probe it first: if something answers, refuse to start (two
    // daemons would reintroduce the multi-writer problem we just removed); if nothing
    // answers, the file is stale and safe to clear.
    if sock.exists() {
        if UnixStream::connect(&sock).is_ok() {
            return Err(io::Error::other(format!(
                "another paosd is already listening on {}",
                sock.display()
            )));
        }
        let _ = std::fs::remove_file(&sock);
    }
    // The dashboard shares this process: no second LaunchAgent, no bun, no build step.
    // It gets its own thread and its own read-only view of the same connection.
    {
        let web = paos_web::Web {
            conn: Arc::clone(&conn),
            embedder: Arc::clone(&embedder),
        };
        let port: u16 = std::env::var("PAOS_UI_PORT").ok()
            .and_then(|p| p.parse().ok()).unwrap_or(8788);
        match std::net::TcpListener::bind(("127.0.0.1", port)) {
            Ok(l) => {
                eprintln!("paosd: dashboard on http://127.0.0.1:{port}");
                std::thread::spawn(move || paos_web::http::serve(l, move |r| web.route(r)));
            }
            // A busy port must not stop the bus from working — the old ai.paos.ui
            // LaunchAgent may still be holding it during cutover.
            Err(e) => eprintln!("paosd: dashboard disabled ({e})"),
        }
    }

    // Telegram bridge. Unconfigured is fine and common: the bus and memory work with
    // no Telegram at all, so an absent token must not stop the daemon.
    {
        // `~/.paos/.env` FIRST — beside the store, where `paos init` writes it and where
        // nothing else syncs. The old location is inside the SKILL directory, which is a
        // deploy target: on 2026-08-03 a skill deploy with `rsync --delete` removed the
        // operator's Telegram credentials, because a secret was living somewhere that
        // gets replaced. The skill path stays as a fallback so an existing machine keeps
        // working, and `paos init` writes to the new one.
        let home = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default());
        let env_path = [home.join(".paos/.env"), home.join(".claude/skills/paos/.env")]
            .into_iter()
            .find(|p| p.exists())
            .unwrap_or_else(|| home.join(".paos/.env"));
        // The database first, .env second. The lock is taken and released before
        // spawning: the bridge takes it itself on every update, and holding it across
        // the spawn would hand the new thread a mutex its own first act is to acquire.
        let tg = {
            let g = conn.lock().unwrap_or_else(|p| p.into_inner());
            paos_operator::telegram::Config::from_db_or_env(&g, &env_path)
        };
        match tg {
            // Deliberately NOT announcing "active" here. spawn() can still refuse — an
            // uninstalled binary must not bridge — and printing the claim first put
            // "telegram bridge active" one line ABOVE "bridge disabled" in the log,
            // which is a log that lies to whoever stops reading at the good news.
            Some(cfg) => bridge::spawn(Arc::clone(&conn), cfg, Arc::clone(&embedder)),
            None => eprintln!("paosd: telegram not configured — bridge disabled"),
        }
    }

    // The nightly dream. Independent of Telegram: memory upkeep should not stop because
    // the phone bridge is unconfigured.
    dream::spawn(Arc::clone(&conn), Arc::clone(&embedder));

    let listener = UnixListener::bind(&sock)?;
    eprintln!("paosd: listening on {}", sock.display());

    let broadcast_wakes = std::env::var("PAOS_BUS_ALL_WAKES")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);
    let daemon = Arc::new(Daemon {
        conn: Arc::clone(&conn),
        push: Registry::new(),
        broadcast_wakes,
        embedder: Arc::clone(&embedder),
        reranker: reranker.clone(),
    });

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let d = Arc::clone(&daemon);
                // One thread per connection. Requests are short request/reply; a
                // blocking listener will get its own thread and that is fine here.
                std::thread::spawn(move || {
                    if let Err(e) = serve(&d, s) {
                        // A client hanging up mid-request is normal, not worth shouting.
                        if e.kind() != io::ErrorKind::UnexpectedEof
                            && e.kind() != io::ErrorKind::BrokenPipe
                        {
                            eprintln!("paosd: connection error: {e}");
                        }
                    }
                });
            }
            Err(e) => eprintln!("paosd: accept failed: {e}"),
        }
    }
    Ok(())
}

fn serve(daemon: &Daemon, mut stream: UnixStream) -> io::Result<()> {
    let req: Request = read_frame(&mut stream)?;
    // `Listen` is the one long-lived request: the connection stays open and this thread
    // blocks on the channel until something warrants a wake. That block costs nothing —
    // no timer, no query — which is the entire point of the rewrite.
    if let Request::Listen { name, rooms, urgent_only } = req {
        return handlers::listen(daemon, &mut stream, &name, rooms, urgent_only);
    }
    let res = handlers::dispatch(daemon, req);
    write_frame(&mut stream, &res)?;
    let _ = stream.shutdown(Shutdown::Write);
    Ok(())
}
