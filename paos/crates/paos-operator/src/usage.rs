//! Fetching Claude subscription usage, and shaping it into the cache everything reads.
//!
//! Ported from `claude_accounts.py`. HTTP is `curl` via `Command`, following
//! `telegram.rs` rather than adding a dependency — one HTTP idiom in this tree, and the
//! single-binary property preserved.
//!
//! **Fail-soft per account is the whole design.** One account whose token cannot refresh
//! must not blank the cache for the others: the poller writes what it has and records the
//! error against that row. A cache that goes empty because one account is broken looks
//! exactly like "no accounts configured", which is what a healthy machine with no slots
//! looks like — and that is the shape the fleet reads to decide whether to switch.

use std::process::Command;

pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// Moved from console.anthropic.com on 2026-07-24: the old host answers 429/404 shims for
/// every refresh. Keep in step with what the `claude` CLI itself sends.
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// A 429 from the token endpoint is per-account and SUSTAINED BY RETRIES: without a
/// pause, the poller re-attempts every 180s and the account never recovers. Doubles per
/// consecutive 429, capped.
pub const REFRESH_BACKOFF_BASE: i64 = 600;
pub const REFRESH_BACKOFF_CAP: i64 = 3600;

/// One usage window as the cache stores it.
///
/// `util` is `Option` because a window the API omits must stay ABSENT rather than
/// becoming 0. Zero means "this account has used nothing", which would make an unknown
/// window look like the most attractive switch target on the machine.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub util: Option<f64>,
    pub resets_at: Option<String>,
}

/// Extract a window from the API's shape (`utilization` / `resets_at`).
pub fn window_of(v: Option<&serde_json::Value>) -> Option<Window> {
    let d = v?.as_object()?;
    Some(Window {
        util: d.get("utilization").and_then(|x| x.as_f64()),
        resets_at: d.get("resets_at").and_then(|x| x.as_str()).map(str::to_string),
    })
}

/// Render a window the way the cache stores it, or `null`.
fn window_json(w: Option<Window>) -> serde_json::Value {
    match w {
        None => serde_json::Value::Null,
        Some(w) => serde_json::json!({ "util": w.util, "resetsAt": w.resets_at }),
    }
}

/// A failed HTTP call, carrying the status when there was one.
///
/// The status is not decoration: `429` is what arms the refresh backoff, and a caller
/// that cannot see it re-attempts every poll against an endpoint that is already
/// rate-limiting it — the exact hot loop `REFRESH_BACKOFF_BASE` exists to prevent.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpFail {
    /// `None` for a transport failure, where no response was received at all.
    pub status: Option<u16>,
    pub message: String,
}

impl HttpFail {
    pub fn is_rate_limited(&self) -> bool {
        self.status == Some(429)
    }
}

impl std::fmt::Display for HttpFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Split curl's `-w '\n%{http_code}'` trailer off the body.
///
/// The code is appended AFTER the body, so split from the right — a JSON body containing
/// newlines is normal and splitting from the left would truncate it.
pub fn split_status(raw: &str) -> (&str, Option<u16>) {
    match raw.rsplit_once('\n') {
        Some((body, code)) => (body, code.trim().parse().ok()),
        None => (raw, None),
    }
}

/// First 200 characters of a response body, on one line — enough to name the cause.
fn snippet(body: &str) -> String {
    let flat: String = body.trim().chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect();
    if flat.chars().count() > 200 {
        format!("{}…", flat.chars().take(200).collect::<String>())
    } else {
        flat
    }
}

/// Run a curl invocation and parse the response, FAILING ON A NON-2XX STATUS.
///
/// **curl exits 0 on an HTTP error** — measured, not assumed: `curl -sS` against a 401
/// returns exit 0 with the error body on stdout, which parses as perfectly good JSON. The
/// Python this replaces used `urlopen`, which RAISES on non-2xx, so checking only the
/// process exit code silently changes the contract. Two consequences, both quiet: a
/// revoked account's row would carry null windows and no `error`, so it reads as an idle
/// account rather than a broken one; and a 429 from the token endpoint would look like a
/// successful refresh, leaving the backoff disarmed forever.
///
/// The message text DIVERGES from the Python by design. `urllib` produced
/// `HTTPError: HTTP Error 429: Too Many Requests` and discarded the body; this keeps the
/// numeric status and appends the body snippet, which is where these APIs put the actual
/// reason. Nothing parses this string — it is rendered for a human in the dashboard and
/// the cache — so the shape of the row matters and the wording does not.
fn send(cmd: &mut Command) -> Result<serde_json::Value, HttpFail> {
    let out = cmd
        .args(["-sS", "--max-time", "20", "-w", "\n%{http_code}"])
        .output()
        .map_err(|e| HttpFail { status: None, message: format!("curl: {e}") })?;
    if !out.status.success() {
        return Err(HttpFail {
            status: None,
            message: format!(
                "curl exited {}: {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        });
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let (body, status) = split_status(&raw);
    if let Some(code) = status {
        if !(200..300).contains(&code) {
            return Err(HttpFail {
                status: Some(code),
                message: format!("HTTPError: HTTP Error {code}: {}", snippet(body)),
            });
        }
    }
    serde_json::from_str(body).map_err(|e| HttpFail {
        status,
        message: format!("response was not JSON: {e}"),
    })
}

/// GET the usage document with a bearer token.
pub fn fetch_usage(access_token: &str, ua: &str) -> Result<serde_json::Value, HttpFail> {
    send(
        Command::new("curl")
            .args(["-X", "GET"])
            .args(["-H", &format!("Authorization: Bearer {access_token}")])
            .args(["-H", "anthropic-beta: oauth-2025-04-20"])
            .args(["-H", &format!("User-Agent: {ua}")])
            .args(["-H", "Content-Type: application/json"])
            .arg(USAGE_URL),
    )
}

/// Exchange a refresh token for a fresh access token.
///
/// Returns the RAW token response; `merge_refreshed` folds it into a credential blob.
/// Keeping them apart is what lets the merge be tested without a network, and the merge
/// is where the field-preservation rule lives.
pub fn oauth_refresh(refresh_token: &str, ua: &str) -> Result<serde_json::Value, HttpFail> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLIENT_ID,
    })
    .to_string();
    send(
        Command::new("curl")
            .args(["-X", "POST"])
            .args(["-H", "Content-Type: application/json"])
            .args(["-H", &format!("User-Agent: {ua}")])
            .args(["-H", "anthropic-beta: oauth-2025-04-20"])
            .args(["--data-binary", &body])
            .arg(TOKEN_URL),
    )
}

/// The User-Agent the `claude` CLI itself sends.
///
/// A non-CLI UA gets the token endpoint 429'd, so this is load-bearing rather than
/// cosmetic. `0.0.0` when the CLI is absent or unparseable — the Python's fallback, kept
/// because a machine without `claude` on PATH must still poll rather than fail.
pub fn user_agent() -> String {
    static CACHE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            let ver = Command::new("claude")
                .arg("--version")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| first_semver(&String::from_utf8_lossy(&o.stdout)))
                .unwrap_or_else(|| "0.0.0".to_string());
            format!("claude-cli/{ver} (external, cli)")
        })
        .clone()
}

/// The first `N.N.N` in a string — the Python's `(\d+\.\d+\.\d+)`, without a regex crate.
pub fn first_semver(s: &str) -> Option<String> {
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        let mut dots = 0;
        let mut j = i;
        let mut last_was_digit = false;
        while j < b.len() {
            if b[j].is_ascii_digit() {
                last_was_digit = true;
                j += 1;
            } else if b[j] == '.' && last_was_digit && dots < 2 {
                dots += 1;
                last_was_digit = false;
                j += 1;
            } else {
                break;
            }
        }
        if dots == 2 && last_was_digit {
            return Some(b[start..j].iter().collect());
        }
        // Skip the whole run we just rejected, or "1.2" inside "1.2.3" rescans forever.
        i = if j > start { j } else { start + 1 };
    }
    None
}

/// Does this credential blob need refreshing before use?
///
/// The 60-SECOND MARGIN is the point: a token that expires in 40s passes a bare
/// `exp > now` check and then 401s mid-request, which surfaces as a fetch error against
/// a perfectly healthy account — and an errored row is excluded from the picker, so a
/// near-expiry token could take an account out of the running for no reason.
///
/// A blob with no expiry is treated as needing one; a blob with no refresh token cannot
/// be refreshed and is used as-is rather than failed, because the LIVE account's blob is
/// kept fresh by Claude Code itself and must never be touched here.
pub fn needs_refresh(blob: &serde_json::Value, now_ms: i64) -> bool {
    let oauth = blob.get("claudeAiOauth");
    let exp = oauth.and_then(|o| o.get("expiresAt")).and_then(|x| x.as_i64()).unwrap_or(0);
    exp == 0 || exp - now_ms <= 60_000
}

/// The refreshed blob, with non-token fields PRESERVED.
///
/// `subscriptionType`, `scopes` and anything else the blob carries must survive a
/// refresh: the token response contains only tokens, so overlaying it wholesale would
/// silently drop fields that other readers depend on. Merge, then overlay.
pub fn merge_refreshed(old: &serde_json::Value, resp: &serde_json::Value, now_ms: i64)
    -> serde_json::Value
{
    let mut oauth = old.get("claudeAiOauth").and_then(|o| o.as_object()).cloned()
        .unwrap_or_default();
    let old_rt = oauth.get("refreshToken").cloned();
    if let Some(at) = resp.get("access_token") {
        oauth.insert("accessToken".into(), at.clone());
    }
    // A refresh response MAY omit refresh_token, in which case the old one stays valid.
    // Dropping it would make the next refresh impossible and the account unpollable.
    let rt = resp.get("refresh_token").cloned().or(old_rt);
    if let Some(rt) = rt {
        oauth.insert("refreshToken".into(), rt);
    }
    let ttl = resp.get("expires_in").and_then(|x| x.as_i64()).unwrap_or(3600);
    oauth.insert("expiresAt".into(), serde_json::json!(now_ms + ttl * 1000));
    serde_json::json!({ "claudeAiOauth": oauth })
}

/// What the backoff state says about a slot right now.
#[derive(Debug, Clone, PartialEq)]
pub enum Backoff {
    /// Free to attempt a refresh.
    Ready,
    /// Rate-limited until this unix time.
    Wait { until: i64, fails: u32 },
}

/// Should this slot attempt a refresh, given its recorded backoff?
///
/// A VALID TOKEN CLEARS THE BACKOFF. That is not an optimisation: a slot that was
/// rate-limited and has since been re-captured by hand (a fresh login) holds a perfectly
/// good token, and leaving the backoff in place would refuse to poll a working account
/// for up to an hour. The backoff describes the REFRESH endpoint's mood, not the
/// account's health, so a token that needs no refresh must not be governed by it.
pub fn backoff_state(blob: &serde_json::Value, recorded: Option<(i64, u32)>, now_ts: i64)
    -> Backoff
{
    if !needs_refresh(blob, now_ts * 1000) {
        return Backoff::Ready;
    }
    match recorded {
        Some((until, fails)) if now_ts < until => Backoff::Wait { until, fails },
        _ => Backoff::Ready,
    }
}

/// The backoff to record after a 429, given how many have preceded it.
pub fn backoff_after_429(previous_fails: u32, now_ts: i64) -> (u32, i64) {
    let fails = previous_fails + 1;
    (fails, now_ts + backoff_secs(fails))
}

/// Build one account row for the cache.
///
/// Kept separate from the fetching so the SHAPE can be tested without a network: the
/// widget, the picker and the dashboard all read these exact keys, and a renamed field
/// is invisible until something downstream quietly reads `null`.
pub fn account_row(
    slot: &str,
    active: bool,
    email: Option<&str>,
    usage: &serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "slot": slot,
        "active": active,
        "email": email,
        "fiveHour":      window_json(window_of(usage.get("five_hour"))),
        "sevenDay":      window_json(window_of(usage.get("seven_day"))),
        "sevenDayOpus":  window_json(window_of(usage.get("seven_day_opus"))),
        "sevenDaySonnet":window_json(window_of(usage.get("seven_day_sonnet"))),
        "extraUsage":    usage.get("extra_usage").cloned().unwrap_or(serde_json::Value::Null),
    })
}

/// A row for an account that could not be polled.
///
/// Carries `error` and NO windows. The picker excludes rows it cannot read rather than
/// treating a missing window as headroom — an errored account with no `util` must never
/// look like the emptiest one.
pub fn error_row(slot: &str, active: bool, email: Option<&str>, err: &str) -> serde_json::Value {
    serde_json::json!({ "slot": slot, "active": active, "email": email, "error": err })
}

/// Assemble the cache document.
pub fn cache_document(polled_at: i64, active: Option<&str>, rows: Vec<serde_json::Value>)
    -> serde_json::Value
{
    serde_json::json!({ "polledAt": polled_at, "active": active, "accounts": rows })
}

/// Seconds to wait before retrying a slot's refresh after `n` consecutive 429s.
pub fn backoff_secs(consecutive: u32) -> i64 {
    if consecutive == 0 {
        return 0;
    }
    let doubled = REFRESH_BACKOFF_BASE.saturating_mul(1i64 << (consecutive - 1).min(20));
    doubled.min(REFRESH_BACKOFF_CAP)
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Serve exactly one HTTP response on a loopback port and return its URL.
    ///
    /// Hermetic on purpose: this exercises the REAL curl invocation, which is where the
    /// status handling lives, without depending on a reachable internet host.
    fn one_shot(status: u16, reason: &str, body: &'static str) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = l.accept() {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(body.as_bytes());
                let _ = s.flush();
            }
        });
        format!("http://127.0.0.1:{port}/")
    }

    #[test]
    fn a_non_2xx_is_an_error_even_though_curl_exits_zero() {
        // THE defect this guards, measured rather than assumed: `curl -sS` against a 401
        // exits 0 and puts a perfectly valid JSON error body on stdout. Checking only the
        // process exit status turns a revoked account into a row with null windows and no
        // `error` — which reads as an account that has used nothing, i.e. the most
        // attractive switch target on the machine.
        let url = one_shot(401, "Unauthorized", r#"{"error":{"message":"revoked"}}"#);
        let err = send(Command::new("curl").arg(&url)).unwrap_err();
        assert_eq!(err.status, Some(401));
        assert!(err.message.contains("401"), "{}", err.message);
        assert!(err.message.contains("revoked"),
                "the body carries the reason urllib used to discard: {}", err.message);
        assert!(!err.is_rate_limited());
    }

    #[test]
    fn a_429_is_distinguishable_because_it_is_what_arms_the_backoff() {
        let url = one_shot(429, "Too Many Requests", r#"{"error":"slow down"}"#);
        let err = send(Command::new("curl").arg(&url)).unwrap_err();
        assert!(err.is_rate_limited(), "status was {:?}", err.status);
    }

    #[test]
    fn a_2xx_parses_and_the_status_trailer_is_not_left_in_the_body() {
        // The `-w` trailer is appended to the body; a naive read would leave "\n200" on
        // the end and fail to parse — or worse, parse and carry it.
        let url = one_shot(200, "OK", "{\"five_hour\":{\"utilization\":12.5}}\n{\"x\":1}");
        // (a body containing a newline, so the right-split is exercised)
        let v = send(Command::new("curl").arg(&url));
        // Two JSON docs is not valid JSON — what matters is that the trailer was stripped
        // before parsing, which the error message shows by naming the SECOND document.
        assert!(v.is_err());

        let url = one_shot(200, "OK", r#"{"five_hour":{"utilization":12.5}}"#);
        let v = send(Command::new("curl").arg(&url)).unwrap();
        assert_eq!(window_of(v.get("five_hour")).unwrap().util, Some(12.5));
    }

    #[test]
    fn the_status_trailer_splits_from_the_right() {
        assert_eq!(split_status("{\"a\":1}\n200"), ("{\"a\":1}", Some(200)));
        // A body with newlines is normal JSON; splitting from the left would truncate it.
        assert_eq!(split_status("line1\nline2\n404"), ("line1\nline2", Some(404)));
        assert_eq!(split_status("\n200"), ("", Some(200)));
        assert_eq!(split_status("no trailer"), ("no trailer", None));
    }

    #[test]
    fn the_user_agent_version_is_the_first_semver_the_cli_prints() {
        // A non-CLI User-Agent gets the token endpoint 429'd, so this is load-bearing.
        assert_eq!(first_semver("2.1.34 (Claude Code)").as_deref(), Some("2.1.34"));
        assert_eq!(first_semver("claude 0.10.7").as_deref(), Some("0.10.7"));
        // A two-part version is not a match, and must not stop the scan finding a real one.
        assert_eq!(first_semver("v1.2 then 3.4.5").as_deref(), Some("3.4.5"));
        assert_eq!(first_semver("1.2.x.3.4.5").as_deref(), Some("3.4.5"));
        assert_eq!(first_semver("no version here").as_deref(), None);
        assert_eq!(first_semver("").as_deref(), None);
        // Longer runs truncate at three parts, exactly as `(\d+\.\d+\.\d+)` does.
        assert_eq!(first_semver("12.34.56.78").as_deref(), Some("12.34.56"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_the_api_omits_stays_absent_rather_than_becoming_zero() {
        // THE TRAP: 0 means "used nothing", which would make an UNKNOWN window look like
        // the most attractive switch target on the machine. Absent must stay absent.
        assert_eq!(window_of(None), None);
        let v = serde_json::json!({});
        assert_eq!(window_of(Some(&v)), Some(Window { util: None, resets_at: None }));
        let v = serde_json::json!({ "utilization": 0.0 });
        assert_eq!(window_of(Some(&v)).unwrap().util, Some(0.0),
                   "an explicit zero is real and must NOT be confused with absent");
    }

    #[test]
    fn a_window_carries_its_reset_time_through() {
        let v = serde_json::json!({ "utilization": 42.5, "resets_at": "2026-08-03T00:00:00Z" });
        let w = window_of(Some(&v)).unwrap();
        assert_eq!(w.util, Some(42.5));
        assert_eq!(w.resets_at.as_deref(), Some("2026-08-03T00:00:00Z"));
    }

    #[test]
    fn an_account_row_carries_every_key_the_widget_and_picker_read() {
        // These names are a contract: the Übersicht widget polls them every 5s, the
        // picker keys off fiveHour/sevenDay, and a renamed field is invisible until
        // something downstream quietly reads null.
        let usage = serde_json::json!({
            "five_hour":  { "utilization": 13.0, "resets_at": "t1" },
            "seven_day":  { "utilization": 100.0, "resets_at": "t2" },
            "seven_day_opus":   { "utilization": 5.0 },
            "seven_day_sonnet": { "utilization": 7.0 },
            "extra_usage": { "any": "shape" }
        });
        let row = account_row("s", true, Some("e@x.com"), &usage);
        for k in ["slot","active","email","fiveHour","sevenDay","sevenDayOpus",
                  "sevenDaySonnet","extraUsage"] {
            assert!(row.get(k).is_some(), "missing key {k}");
        }
        assert_eq!(row["fiveHour"]["util"], serde_json::json!(13.0));
        assert_eq!(row["sevenDay"]["resetsAt"], serde_json::json!("t2"));
        assert_eq!(row["active"], serde_json::json!(true));
    }

    #[test]
    fn the_row_shape_matches_the_cache_the_python_actually_wrote() {
        // Compared against the LIVE cache on this machine, not against my reading of
        // poll_all: top keys, account keys and window keys, all three. The Übersicht
        // widget polls these every 5 seconds and the picker keys off them, so a renamed
        // field is invisible until something downstream quietly reads null.
        let usage = serde_json::json!({ "five_hour": {"utilization": 1.0, "resets_at": "t"} });
        let row = account_row("s", false, Some("e"), &usage);
        let mut got: Vec<&str> = row.as_object().unwrap().keys().map(String::as_str).collect();
        got.sort();
        assert_eq!(got, ["active","email","extraUsage","fiveHour","sevenDay",
                         "sevenDayOpus","sevenDaySonnet","slot"]);

        let mut w: Vec<&str> = row["fiveHour"].as_object().unwrap().keys()
            .map(String::as_str).collect();
        w.sort();
        assert_eq!(w, ["resetsAt","util"]);

        let doc = cache_document(1, Some("s"), vec![row]);
        let mut t: Vec<&str> = doc.as_object().unwrap().keys().map(String::as_str).collect();
        t.sort();
        assert_eq!(t, ["accounts","active","polledAt"]);
    }

    #[test]
    fn an_errored_account_carries_no_windows_at_all() {
        // The picker must EXCLUDE what it cannot read. A row with error and no util must
        // never be mistaken for the emptiest account on the machine.
        let row = error_row("broken", false, Some("e@x.com"), "TimeoutError: ...");
        assert!(row.get("error").is_some());
        assert!(row.get("fiveHour").is_none() && row.get("sevenDay").is_none());
    }

    #[test]
    fn the_cache_document_has_the_three_top_level_keys_readers_expect() {
        let doc = cache_document(1234, Some("a"), vec![serde_json::json!({"slot":"a"})]);
        assert_eq!(doc["polledAt"], serde_json::json!(1234));
        assert_eq!(doc["active"], serde_json::json!("a"));
        assert_eq!(doc["accounts"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_token_expiring_within_the_minute_counts_as_expired() {
        // A token with 40s left passes a bare `exp > now` check and then 401s MID-REQUEST.
        // That surfaces as a fetch error against a healthy account, and an errored row is
        // excluded from the picker — so a near-expiry token could take an account out of
        // the running for no reason at all.
        let blob = |exp: i64| serde_json::json!({ "claudeAiOauth": { "expiresAt": exp } });
        let now = 1_000_000_000i64;
        assert!(!needs_refresh(&blob(now + 120_000), now), "two minutes of life is fine");
        assert!(needs_refresh(&blob(now + 40_000), now), "40s would 401 mid-request");
        assert!(needs_refresh(&blob(now + 60_000), now), "exactly the margin is not enough");
        assert!(needs_refresh(&blob(0), now), "no expiry means refresh");
        assert!(needs_refresh(&serde_json::json!({}), now), "no oauth block means refresh");
    }

    #[test]
    fn a_refresh_preserves_the_fields_the_token_response_does_not_carry() {
        // subscriptionType, scopes and anything else must survive. The token response
        // contains only tokens, so overlaying it wholesale drops fields other readers
        // depend on — silently, because they simply become absent.
        let old = serde_json::json!({ "claudeAiOauth": {
            "accessToken": "old-at", "refreshToken": "old-rt", "expiresAt": 1,
            "subscriptionType": "max", "scopes": ["a","b"] }});
        let resp = serde_json::json!({ "access_token": "new-at", "expires_in": 100 });
        let merged = merge_refreshed(&old, &resp, 5_000);
        let o = &merged["claudeAiOauth"];
        assert_eq!(o["accessToken"], serde_json::json!("new-at"));
        assert_eq!(o["subscriptionType"], serde_json::json!("max"), "must survive");
        assert_eq!(o["scopes"], serde_json::json!(["a","b"]), "must survive");
        assert_eq!(o["expiresAt"], serde_json::json!(5_000 + 100 * 1000));
        // The response omitted refresh_token, so the OLD one must be kept — dropping it
        // makes the next refresh impossible and the account permanently unpollable.
        assert_eq!(o["refreshToken"], serde_json::json!("old-rt"));
    }

    #[test]
    fn a_rotated_refresh_token_replaces_the_old_one() {
        let old = serde_json::json!({ "claudeAiOauth": { "refreshToken": "old-rt" }});
        let resp = serde_json::json!({ "access_token": "a", "refresh_token": "new-rt" });
        let merged = merge_refreshed(&old, &resp, 0);
        assert_eq!(merged["claudeAiOauth"]["refreshToken"], serde_json::json!("new-rt"));
    }

    #[test]
    fn a_valid_token_clears_a_recorded_backoff() {
        // NOT an optimisation. A slot that was rate-limited and has since been re-captured
        // by hand holds a perfectly good token; leaving the backoff in place would refuse
        // to poll a WORKING account for up to an hour. The backoff describes the refresh
        // endpoint's mood, not the account's health.
        let now = 1_000_000i64;
        let good = serde_json::json!({ "claudeAiOauth": { "expiresAt": (now + 3600) * 1000 }});
        assert_eq!(backoff_state(&good, Some((now + 900, 3)), now), Backoff::Ready,
                   "a token that needs no refresh must not be governed by the backoff");
    }

    #[test]
    fn an_expiring_token_respects_the_backoff_window_and_then_leaves_it() {
        let now = 1_000_000i64;
        let stale = serde_json::json!({ "claudeAiOauth": { "expiresAt": now * 1000 }});
        assert_eq!(backoff_state(&stale, Some((now + 300, 2)), now),
                   Backoff::Wait { until: now + 300, fails: 2 });
        // Past the window it is free to try again — the backoff must EXPIRE, or one 429
        // takes an account out permanently.
        assert_eq!(backoff_state(&stale, Some((now - 1, 2)), now), Backoff::Ready);
        assert_eq!(backoff_state(&stale, None, now), Backoff::Ready);
    }

    #[test]
    fn consecutive_429s_push_the_window_out_and_then_stop() {
        let now = 0i64;
        assert_eq!(backoff_after_429(0, now), (1, REFRESH_BACKOFF_BASE));
        assert_eq!(backoff_after_429(1, now), (2, REFRESH_BACKOFF_BASE * 2));
        // Capped, so an account that 429s repeatedly is still polled eventually.
        assert_eq!(backoff_after_429(30, now), (31, REFRESH_BACKOFF_CAP));
    }

    #[test]
    fn the_backoff_doubles_and_then_stops_doubling() {
        // A 429 is sustained BY retrying, so this exists to let an account recover. The
        // cap matters as much as the growth: unbounded doubling means an account that
        // 429s a dozen times is never polled again.
        assert_eq!(backoff_secs(0), 0);
        assert_eq!(backoff_secs(1), REFRESH_BACKOFF_BASE);
        assert_eq!(backoff_secs(2), REFRESH_BACKOFF_BASE * 2);
        assert_eq!(backoff_secs(3), REFRESH_BACKOFF_BASE * 4);
        assert_eq!(backoff_secs(50), REFRESH_BACKOFF_CAP, "capped, and no overflow");
    }
}

