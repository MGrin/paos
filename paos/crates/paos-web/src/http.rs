//! A minimal HTTP/1.1 server on std only.
//!
//! No axum, no hyper, no tokio. This serves one user on localhost; the whole surface is
//! a dozen GET routes and a handful of POSTs. A framework would add hundreds of crates
//! and an async runtime to a binary whose entire job is reading a local SQLite file.
//!
//! It replaces a stack that was, measured: **277 MB of `next/` + `@next/` inside a
//! 443 MB `node_modules`, plus a 403 MB `.next` cache, to emit 1.2 MB of JS** — for an
//! app with `output: "export"`, zero server components, and 36 of 44 files marked
//! `"use client"`. Next.js was a build-time bundler that also required a separate bun
//! process to serve the result, and the LaunchAgent never ran the build, so an edited
//! component silently served stale assets forever.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub body: String,
    /// `Origin` as sent, if any. Browsers ALWAYS attach this to a cross-origin POST,
    /// including a plain form submission, which is what makes it a usable CSRF signal.
    pub origin: Option<String>,
    /// `Sec-Fetch-Site`, sent by current browsers: `same-origin`, `cross-site`, `none`.
    pub fetch_site: Option<String>,
}

impl Request {
    /// Is this a state-changing request a WEBSITE could have forged?
    ///
    /// The dashboard listens on localhost with no authentication, and every write
    /// endpoint here is destructive or authoritative: forget a memory, approve a memory
    /// change, switch the active Claude account, alter daemon config — and `/api/answer`,
    /// which speaks to sessions AS THE OPERATOR. Any page in any browser could POST to
    /// 127.0.0.1:8788; a form POST is a "simple request", so there is no preflight to
    /// stop it and the attacker never needs to read the response.
    ///
    /// A localhost-only bind is not protection: the threat is the operator's own browser
    /// being used as the confused deputy.
    ///
    /// Rule: a request carrying browser provenance must prove it came from this page.
    /// Requests with no such headers (curl, the CLI, a script) are allowed — they are not
    /// reachable from a web page in the first place.
    pub fn is_forgeable(&self) -> bool {
        if self.method != "POST" {
            return false;
        }
        if let Some(site) = self.fetch_site.as_deref() {
            if site != "same-origin" && site != "none" {
                return true;
            }
        }
        match self.origin.as_deref() {
            None => false,
            Some(o) => !matches!(
                o,
                "http://127.0.0.1:8788" | "http://localhost:8788"
            ),
        }
    }
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(body: String) -> Self {
        Self { status: 200, content_type: "application/json; charset=utf-8", body: body.into_bytes() }
    }
    pub fn html(body: &str) -> Self {
        Self { status: 200, content_type: "text/html; charset=utf-8", body: body.as_bytes().to_vec() }
    }
    /// Static assets compiled into the binary. No cache header on purpose: the binary IS
    /// the cache, so a rebuilt daemon must never serve a browser-cached older board.
    pub fn css(body: &str) -> Self {
        Self { status: 200, content_type: "text/css; charset=utf-8", body: body.as_bytes().to_vec() }
    }
    pub fn js(body: &str) -> Self {
        Self { status: 200, content_type: "text/javascript; charset=utf-8",
               body: body.as_bytes().to_vec() }
    }
    pub fn text(status: u16, body: &str) -> Self {
        Self { status, content_type: "text/plain; charset=utf-8", body: body.as_bytes().to_vec() }
    }
    pub fn not_found() -> Self {
        Self::text(404, "not found")
    }
}

/// Cap request bodies. A localhost tool has no reason to accept a large upload, and an
/// unbounded read is an easy way to be wedged by a stuck client.
const MAX_BODY: usize = 1 << 20;

pub fn serve<F>(listener: TcpListener, handler: F) -> !
where
    F: Fn(&Request) -> Response + Send + Sync + 'static,
{
    let handler = std::sync::Arc::new(handler);
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let h = std::sync::Arc::clone(&handler);
        std::thread::spawn(move || {
            if let Err(e) = handle(stream, h.as_ref()) {
                // A browser closing a tab mid-request is routine.
                if e.kind() != std::io::ErrorKind::BrokenPipe {
                    eprintln!("paos-web: {e}");
                }
            }
        });
    }
    unreachable!("incoming() never terminates")
}

fn handle<F>(mut stream: TcpStream, handler: &F) -> std::io::Result<()>
where
    F: Fn(&Request) -> Response,
{
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let raw_path = parts.next().unwrap_or("/").to_string();

    let mut content_length = 0usize;
    let (mut origin, mut fetch_site) = (None, None);
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
        let lower = h.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        } else if lower.starts_with("origin:") {
            origin = h[7..].trim().to_string().into();
        } else if lower.starts_with("sec-fetch-site:") {
            fetch_site = h[15..].trim().to_ascii_lowercase().into();
        }
    }

    let mut body = String::new();
    if content_length > 0 {
        let want = content_length.min(MAX_BODY);
        let mut buf = vec![0u8; want];
        reader.read_exact(&mut buf)?;
        body = String::from_utf8_lossy(&buf).into_owned();
    }

    let (path, query) = split_query(&raw_path);
    let req = Request { method, path, query, body, origin, fetch_site };
    let res = handler(&req);

    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        res.status,
        reason(res.status),
        res.content_type,
        res.body.len()
    )?;
    stream.write_all(&res.body)?;
    stream.flush()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "OK",
    }
}

fn split_query(raw: &str) -> (String, HashMap<String, String>) {
    let (path, qs) = match raw.split_once('?') {
        Some((p, q)) => (p, q),
        None => (raw, ""),
    };
    let mut map = HashMap::new();
    for pair in qs.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(percent_decode(k), percent_decode(v));
    }
    (percent_decode(path), map)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Minimal JSON string escaping.
///
/// Hand-rolled rather than pulling serde_json into the render path: every value here is
/// a database string, and the failure mode of getting this wrong (a quote or newline in
/// a memory breaking the page) is worth an explicit, tested function.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_path_and_query() {
        let (p, q) = split_query("/api/recall?q=hello%20world&k=5");
        assert_eq!(p, "/api/recall");
        assert_eq!(q.get("q").map(String::as_str), Some("hello world"));
        assert_eq!(q.get("k").map(String::as_str), Some("5"));
    }

    #[test]
    fn path_without_query_is_untouched() {
        let (p, q) = split_query("/");
        assert_eq!(p, "/");
        assert!(q.is_empty());
    }

    #[test]
    fn decodes_plus_and_percent_escapes() {
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("%2Fetc%2Fpasswd"), "/etc/passwd");
        assert_eq!(percent_decode("caf%C3%A9"), "café");
    }

    #[test]
    fn malformed_percent_escape_does_not_panic() {
        // A browser or a fuzzer will send these; dropping the request is fine, crashing
        // the thread is not.
        assert_eq!(percent_decode("%"), "%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn json_escaping_survives_real_memory_text() {
        // Memories genuinely contain quotes, backslashes and newlines.
        assert_eq!(esc(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(esc("a\nb"), "a\\nb");
        assert_eq!(esc(r"C:\path"), r"C:\\path");
        assert_eq!(esc("tab\there"), "tab\\there");
    }

    #[test]
    fn control_characters_are_escaped_not_emitted_raw() {
        // A raw control byte makes the JSON unparseable and blanks the page.
        let out = esc("bell\u{7}end");
        assert!(out.contains("\\u0007"), "got {out}");
    }

    #[test]
    fn unicode_passes_through_intact() {
        assert_eq!(esc("⚙ operator mode → away"), "⚙ operator mode → away");
    }
}
