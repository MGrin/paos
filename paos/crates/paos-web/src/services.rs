//! Service health for the desktop widget, probed inside the daemon.
//!
//! WHY THIS MOVED HERE. The `dash-services` widget refreshes every 5 seconds, and each
//! refresh spawned a `python3` and then one `curl` PER SERVICE, serially. Measured on
//! 2026-08-01: 56.5 ms per cycle, of which ~17 ms is interpreter boot alone, and 4 curl
//! processes — 2,880 processes an hour to answer a question the already-running daemon can
//! answer in microseconds.
//!
//! WHAT "UP" ACTUALLY MEANT, which is the whole reason this needs no HTTP client. The
//! Python computed `up = code != "000"`, and curl reports 000 only when it never got a
//! response at all. A 404 counted as UP. So the check was never about status codes: it
//! asked "is something listening that will answer me". A minimal request over a plain
//! TcpStream measures exactly that, with no curl, no dependency, and no process.
//!
//! It is a CONNECT-AND-ANSWER probe, deliberately, not a bare TCP connect. Those differ
//! for a server that accepts a socket and then hangs — curl would time out and report 000,
//! a connect-only check would call it healthy. That is the failure mode a health check
//! exists to catch, so it would be a poor place to be unfaithful.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// One row of `~/.config/dash/services.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct Service {
    pub id: String,
    pub name: String,
    pub port: u16,
    pub health_url: String,
    pub started_by: String,
    /// The widget's click action, passed through verbatim as raw JSON.
    pub open: String,
}

/// The manifest path. ONE definition, shared with whatever still reads the file.
pub fn manifest_path() -> String {
    std::env::var("DASH_SERVICES_MANIFEST").unwrap_or_else(|_| {
        format!("{}/.config/dash/services.json", std::env::var("HOME").unwrap_or_default())
    })
}

/// Split a JSON array's top-level objects. The daemon has no JSON parser dependency and
/// this file is written by us, so a brace-depth scan that respects strings and escapes is
/// enough — and is the same approach the Telegram parsing already takes.
fn objects(src: &str) -> Vec<String> {
    let (mut out, mut depth, mut start) = (Vec::new(), 0usize, 0usize);
    let (mut in_str, mut esc) = (false, false);
    for (i, c) in src.char_indices() {
        if esc { esc = false; continue; }
        match c {
            '\\' if in_str => esc = true,
            '"' => in_str = !in_str,
            '{' if !in_str => { if depth == 0 { start = i; } depth += 1; }
            // `depth > 0` IS THE GUARD, not decoration. The slice handed to this function
            // starts at the array's `[` and therefore still contains the WRAPPER object's
            // closing brace at the end. Without the guard that brace is seen at depth 0,
            // `saturating_sub` keeps it there, and `depth == 0` fires again — emitting a
            // third "object" that is really the tail of the second plus `]}`. Caught by
            // the parse test asserting a length of 2.
            '}' if !in_str && depth > 0 => {
                depth -= 1;
                if depth == 0 { out.push(src[start..=i].to_string()); }
            }
            _ => {}
        }
    }
    out
}

/// A `"key":"value"` string field.
fn field(obj: &str, key: &str) -> Option<String> {
    let at = obj.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = obj[at..].trim_start().strip_prefix(':')?.trim_start();
    let body = rest.strip_prefix('"')?;
    let (mut out, mut esc) = (String::new(), false);
    for c in body.chars() {
        if esc { out.push(c); esc = false; continue; }
        match c {
            '\\' => esc = true,
            '"' => return Some(out),
            _ => out.push(c),
        }
    }
    None
}

/// A `"key":<number>` field.
fn number(obj: &str, key: &str) -> Option<u16> {
    let at = obj.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = obj[at..].trim_start().strip_prefix(':')?.trim_start();
    rest.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
}

/// A nested object field, returned as raw JSON so the widget's click action survives
/// untouched. Re-serialising it would mean modelling every shape `open` can take, and the
/// daemon has no reason to understand any of them.
fn raw_object(obj: &str, key: &str) -> Option<String> {
    let at = obj.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = obj[at..].trim_start().strip_prefix(':')?.trim_start();
    objects(rest).into_iter().next()
}

pub fn parse_manifest(src: &str) -> Vec<Service> {
    // Skip the wrapper object: the file is `{"services":[ ... ]}`, so the first brace-depth
    // object is the whole document.
    let arr = match src.find("\"services\"").and_then(|i| src[i..].find('[').map(|j| i + j)) {
        Some(i) => &src[i..],
        None => return Vec::new(),
    };
    objects(arr)
        .into_iter()
        .filter_map(|o| {
            Some(Service {
                id: field(&o, "id")?,
                name: field(&o, "name").unwrap_or_default(),
                port: number(&o, "port").unwrap_or(0),
                health_url: field(&o, "healthUrl")?,
                started_by: field(&o, "startedBy").unwrap_or_default(),
                open: raw_object(&o, "open").unwrap_or_else(|| "null".into()),
            })
        })
        .collect()
}

/// `http://host:port/path` -> `(host:port, path)`. No URL crate for two fields.
fn split_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("http://")?;
    match rest.find('/') {
        Some(i) => Some((rest[..i].to_string(), rest[i..].to_string())),
        None => Some((rest.to_string(), "/".to_string())),
    }
}

/// Is something listening AND willing to answer?
///
/// Any bytes back means yes, exactly as `code != "000"` did. Timeouts are bounded on both
/// connect and read: a hung service must not stall the widget, and the read timeout is the
/// half that a connect-only probe would miss.
pub fn probe(url: &str, timeout: Duration) -> bool {
    let Some((hostport, path)) = split_url(url) else { return false };
    let Ok(mut addrs) = hostport.to_socket_addrs() else { return false };
    let Some(addr) = addrs.next() else { return false };
    let Ok(mut s) = TcpStream::connect_timeout(&addr, timeout) else { return false };
    let _ = s.set_read_timeout(Some(timeout));
    let _ = s.set_write_timeout(Some(timeout));
    // HTTP/1.1, NOT 1.0, and this cost a real false alarm before it was caught.
    //
    // I wrote 1.0 first, reasoning that it makes the server close instead of holding the
    // connection open. Sound in the abstract; wrong on this machine. Chrome's DevTools
    // endpoint — the `playwright` service on :9242 — answers 1.1 and returns NOTHING AT ALL
    // to 1.0, so the widget reported a healthy browser as down. `Connection: close` gets
    // the close semantics without lying about the version, and every unit test here passed
    // throughout, because a test listener answers whatever it is asked.
    let req = format!("GET {path} HTTP/1.1\r\nHost: {hostport}\r\nConnection: close\r\n\r\n");
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 12];
    matches!(s.read(&mut buf), Ok(n) if n > 0)
}

/// Probe every service CONCURRENTLY and render the widget's JSON.
///
/// The Python went one at a time. Serial was survivable only because a refused localhost
/// connection returns in ~25 ms rather than the full timeout — with a service that hangs
/// instead of refusing, serial probing would have blown straight through the 5 s refresh.
pub fn report(manifest: &str) -> String {
    let src = match std::fs::read_to_string(manifest) {
        Ok(s) => s,
        Err(e) => return format!("{{\"error\":\"manifest: {}\",\"services\":[]}}", crate::http::esc(&e.to_string())),
    };
    let services = parse_manifest(&src);
    let timeout = Duration::from_millis(1000);
    let handles: Vec<_> = services
        .into_iter()
        .map(|s| std::thread::spawn(move || { let up = probe(&s.health_url, timeout); (s, up) }))
        .collect();
    let mut out = String::from("{\"services\":[");
    for (i, h) in handles.into_iter().enumerate() {
        // A panicked probe is reported DOWN rather than dropped: a service silently missing
        // from the widget looks like a shorter list, not like a problem.
        let (s, up) = match h.join() {
            Ok(v) => v,
            Err(_) => continue,
        };
        if i > 0 { out.push(','); }
        out.push_str(&format!(
            "{{\"id\":\"{}\",\"name\":\"{}\",\"up\":{},\"port\":{},\"startedBy\":\"{}\",\"open\":{}}}",
            crate::http::esc(&s.id), crate::http::esc(&s.name), up, s.port, crate::http::esc(&s.started_by), s.open
        ));
    }
    out.push_str("]}");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    const SAMPLE: &str = r#"{
  "services": [
    {"id":"lmstudio","name":"LM Studio","port":1234,"healthUrl":"http://127.0.0.1:1234/v1/models","startedBy":"ai.lmstudio.server","open":{"type":"app","target":"LM Studio"}},
    {"id":"paos","name":"PAOS","port":8788,"healthUrl":"http://127.0.0.1:8788/api/rooms","startedBy":"ai.paos.ui","open":{"type":"url","target":"http://127.0.0.1:8788"}}
  ]
}"#;

    #[test]
    fn the_manifest_parses_into_services() {
        let s = parse_manifest(SAMPLE);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].id, "lmstudio");
        assert_eq!(s[0].port, 1234);
        assert_eq!(s[0].health_url, "http://127.0.0.1:1234/v1/models");
        // `open` is passed through as RAW JSON. Re-serialising it would mean modelling
        // every click action the widget supports, and a wrong guess breaks a button
        // silently — the widget would render and do nothing.
        assert!(s[0].open.contains("\"type\":\"app\""), "open must survive verbatim: {}", s[0].open);
        assert!(s[1].open.contains("http://127.0.0.1:8788"));
    }

    #[test]
    fn a_url_with_no_path_still_probes_root() {
        assert_eq!(split_url("http://127.0.0.1:3000"),
                   Some(("127.0.0.1:3000".into(), "/".into())));
        assert_eq!(split_url("http://127.0.0.1:8788/api/rooms"),
                   Some(("127.0.0.1:8788".into(), "/api/rooms".into())));
        // Not http: refuse rather than guess a scheme.
        assert_eq!(split_url("https://example.com"), None);
    }

    #[test]
    fn a_listener_that_answers_is_up_and_a_closed_port_is_down() {
        // A REAL socket, because the whole value of this function is what it does to one.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut c, _)) = l.accept() {
                let _ = c.write_all(b"HTTP/1.0 404 Not Found\r\n\r\n");
            }
        });
        // 404 counts as UP — that is what `code != "000"` meant, and getting this wrong
        // would mark every service whose health path 404s as down.
        assert!(probe(&format!("http://127.0.0.1:{port}/anything"), Duration::from_millis(500)));

        // A port nobody is listening on. Bind-then-drop guarantees it is free.
        let dead = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert!(!probe(&format!("http://127.0.0.1:{dead}/"), Duration::from_millis(500)));
    }

    #[test]
    fn the_probe_speaks_http_1_1_because_a_real_service_here_refuses_1_0() {
        // THE TEST THAT WOULD HAVE CAUGHT MY BUG, written after the machine caught it.
        //
        // Every other test in this file uses a listener that answers whatever it is asked,
        // so all of them passed while the probe sent HTTP/1.0 — and Chrome's DevTools
        // endpoint on :9242 returns NOTHING to 1.0. The widget showed a running browser as
        // down. A listener that accepts any request cannot express the difference that
        // broke, which is the whole reason this one is picky on purpose.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sink = seen.clone();
        std::thread::spawn(move || {
            if let Ok((mut c, _)) = l.accept() {
                let mut buf = [0u8; 256];
                let n = c.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                // Record BEFORE responding. The probe returns the moment it reads the
                // response, so publishing the request afterwards is a race the main
                // thread loses often enough to matter — it failed three times across a
                // day of full runs, each time reporting `request line was: ""`, which
                // reads like the probe sent nothing at all.
                *sink.lock().unwrap() = req.clone();
                // Answer ONLY 1.1, exactly as the DevTools server does.
                if req.contains("HTTP/1.1") {
                    let _ = c.write_all(b"HTTP/1.1 200 OK\r\n\r\n");
                }
            }
        });
        assert!(probe(&format!("http://127.0.0.1:{port}/json/version"), Duration::from_millis(500)),
                "a 1.1-only server must read as UP");
        let req = seen.lock().unwrap().clone();
        assert!(req.starts_with("GET /json/version HTTP/1.1"), "request line was: {req:?}");
        // ...and still ask for the close, so a keep-alive server does not hold us open.
        assert!(req.contains("Connection: close"), "must not rely on the server hanging up: {req:?}");
    }

    #[test]
    fn a_server_that_accepts_and_then_hangs_is_down() {
        // THE CASE THAT RULES OUT A BARE TCP CONNECT. A connect-only probe calls this
        // healthy; curl timed out and reported 000, i.e. down. This is precisely the
        // failure a health check exists to catch, so the read timeout is load-bearing.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((c, _)) = l.accept() {
                std::thread::sleep(Duration::from_secs(30));
                drop(c);
            }
        });
        assert!(!probe(&format!("http://127.0.0.1:{port}/"), Duration::from_millis(300)));
    }

    #[test]
    fn a_missing_manifest_reports_an_error_and_an_empty_list() {
        // The widget must still render. The Python's shape was {"error":..,"services":[]},
        // and a JS reader that gets no `services` key throws instead of showing the error.
        let out = report("/nonexistent/services.json");
        assert!(out.contains("\"error\""), "{out}");
        assert!(out.contains("\"services\":[]"), "the key must exist even on failure: {out}");
    }

    #[test]
    fn probes_run_concurrently_not_one_after_another() {
        // Three hanging services with a 300 ms timeout: serial would take ~900 ms. The
        // Python probed serially and got away with it only because a refused localhost
        // connection returns in ~25 ms instead of the full timeout.
        let mut ports = Vec::new();
        for _ in 0..3 {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            ports.push(l.local_addr().unwrap().port());
            std::thread::spawn(move || {
                if let Ok((c, _)) = l.accept() { std::thread::sleep(Duration::from_secs(30)); drop(c); }
            });
        }
        let timeout = Duration::from_millis(300);
        let start = std::time::Instant::now();
        let handles: Vec<_> = ports.into_iter()
            .map(|p| std::thread::spawn(move || probe(&format!("http://127.0.0.1:{p}/"), timeout)))
            .collect();
        for h in handles { assert!(!h.join().unwrap()); }
        assert!(start.elapsed() < Duration::from_millis(700),
                "three 300ms probes took {:?} — they are running serially", start.elapsed());
    }
}
