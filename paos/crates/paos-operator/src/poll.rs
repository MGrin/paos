//! The poll loop: refresh what needs refreshing, fetch usage, assemble the cache.
//!
//! Ported from `poll_all` in `claude_accounts.py`. Everything the loop touches outside
//! itself goes through [`Backend`], so the ORDERING rules below are tested without a
//! network, a keychain, or a real account — which matters because every one of them is a
//! rule about a failure, and failures are exactly what a live test cannot arrange.
//!
//! Three rules the loop exists to enforce:
//!
//! 1. **Fail-soft per account.** One unpollable account records its error in its own row
//!    and the others still poll. A cache that goes empty because one account is broken is
//!    indistinguishable from "no accounts configured".
//! 2. **The ACTIVE account is never refreshed.** Claude Code keeps that credential fresh
//!    itself and rotates the refresh token as it goes; refreshing it from here races the
//!    CLI and can invalidate the token the CLI is holding.
//! 3. **A 429 arms a persisted backoff.** The poller runs every few minutes, so an
//!    un-armed backoff means re-attempting a rate-limited endpoint forever and never
//!    recovering.

use crate::slots;
use crate::usage::{self, Backoff, HttpFail};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Everything [`poll_all`] reaches outside itself.
pub trait Backend {
    fn slots(&self) -> Vec<String>;
    fn active(&self) -> Option<String>;
    /// The stashed identity for a slot. An unreadable identity is an ERROR, not a missing
    /// email: `list_slots` enumerates these files, so one that will not parse means the
    /// slot is damaged and polling it would report a healthy-looking anonymous account.
    fn identity(&self, slot: &str) -> Result<Value, String>;
    fn live_blob(&self) -> Result<Value, String>;
    fn slot_blob(&self, slot: &str) -> Result<Value, String>;
    fn store_blob(&self, slot: &str, blob: &Value) -> Result<(), String>;
    fn refresh(&self, refresh_token: &str) -> Result<Value, HttpFail>;
    fn usage(&self, access_token: &str) -> Result<Value, HttpFail>;
}

/// `~/.config/claude-usage/state.json` — the poller's persisted memory.
pub fn state_path() -> PathBuf {
    slots::config_dir().join("state.json")
}

/// Load the state, or an empty object. A corrupt state file must not stop a poll: the
/// worst it costs is a forgotten backoff, and refusing to poll costs the whole cache.
pub fn load_state_at(p: &Path) -> Value {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}))
}

/// Write the state atomically.
///
/// The state is read by the next poll and by the switcher; a torn write loses
/// `last_switch_ts`, which is what enforces the switch cooldown. Same-directory temp file
/// plus rename, so the replacement is atomic on the filesystem.
pub fn save_state_at(p: &Path, st: &Value) -> Result<(), String> {
    let dir = p.parent().ok_or("state path has no parent")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join(format!(".state.{}.tmp", std::process::id()));
    std::fs::write(&tmp, st.to_string()).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, p).map_err(|e| format!("{}: {e}", p.display()))
}

/// The recorded `(until, fails)` for a slot, if any.
pub fn recorded_backoff(st: &Value, slot: &str) -> Option<(i64, u32)> {
    let b = st.get("refresh_backoff")?.get(slot)?;
    Some((
        b.get("until")?.as_i64()?,
        b.get("fails").and_then(|f| f.as_u64()).unwrap_or(0) as u32,
    ))
}

pub fn set_backoff(st: &mut Value, slot: &str, until: i64, fails: u32) {
    if !st.get("refresh_backoff").map(Value::is_object).unwrap_or(false) {
        st["refresh_backoff"] = serde_json::json!({});
    }
    st["refresh_backoff"][slot] = serde_json::json!({ "fails": fails, "until": until });
}

/// Forget a slot's backoff. A no-op when there is none, so a healthy poll does not grow
/// an empty `refresh_backoff` key into the state file.
pub fn clear_backoff(st: &mut Value, slot: &str) {
    if let Some(map) = st.get_mut("refresh_backoff").and_then(Value::as_object_mut) {
        map.remove(slot);
    }
}

/// Local `HH:MM` for an epoch, for the human reading the error row.
///
/// Shells out to `date` the way `paos-cli` already does rather than pulling in a date
/// crate: this runs only when a slot is actually rate-limited, and the alternative is
/// hand-rolling timezone handling for one error string.
fn hhmm(epoch: i64) -> String {
    std::process::Command::new("date")
        .args(["-r", &epoch.to_string(), "+%H:%M"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| epoch.to_string())
}

/// Refresh a non-active slot's credential, honouring and maintaining its backoff.
fn refresh_with_backoff(
    be: &dyn Backend,
    st: &mut Value,
    slot: &str,
    now_ts: i64,
) -> Result<Value, String> {
    let blob = be.slot_blob(slot)?;
    let now_ms = now_ts * 1000;

    // A VALID TOKEN CLEARS THE BACKOFF: the backoff describes the refresh endpoint's
    // mood, and a slot re-captured by hand needs no refresh at all.
    if !usage::needs_refresh(&blob, now_ms) {
        clear_backoff(st, slot);
        return Ok(blob);
    }
    if let Backoff::Wait { until, .. } =
        usage::backoff_state(&blob, recorded_backoff(st, slot), now_ts)
    {
        return Err(format!(
            "RuntimeError: refresh rate-limited; next attempt after {}",
            hhmm(until)
        ));
    }

    // No refresh token: use the blob as-is rather than failing. The Python does the same,
    // and an errored row is excluded from the switch picker — so failing here would take a
    // possibly-fine account out of the running.
    let Some(rt) = blob
        .get("claudeAiOauth")
        .and_then(|o| o.get("refreshToken"))
        .and_then(|t| t.as_str())
    else {
        return Ok(blob);
    };

    match be.refresh(rt) {
        Ok(resp) => {
            let merged = usage::merge_refreshed(&blob, &resp, now_ms);
            // Stash BEFORE clearing the backoff, and propagate a failed stash. The server
            // may have rotated the refresh token, so a merged blob we could not persist
            // means the stashed copy is now stale — the next refresh will 400
            // `invalid_grant` and the slot needs a manual re-login. Reporting the write
            // failure is the only warning anyone gets.
            be.store_blob(slot, &merged)?;
            clear_backoff(st, slot);
            Ok(merged)
        }
        Err(f) => {
            if f.is_rate_limited() {
                let prev = recorded_backoff(st, slot).map(|(_, fails)| fails).unwrap_or(0);
                let (fails, until) = usage::backoff_after_429(prev, now_ts);
                set_backoff(st, slot, until, fails);
            }
            Err(f.message)
        }
    }
}

/// Poll every slot and return the cache document. Mutates `st` with backoff changes; the
/// caller persists it.
pub fn poll_all(be: &dyn Backend, st: &mut Value, now_ts: i64) -> Value {
    let active = be.active();
    let mut rows = Vec::new();

    for slot in be.slots() {
        let is_active = active.as_deref() == Some(slot.as_str());
        let (email, blob) = match be.identity(&slot) {
            Err(e) => {
                rows.push(usage::error_row(&slot, is_active, None, &e));
                continue;
            }
            Ok(ident) => {
                let email = ident
                    .get("oauthAccount")
                    .and_then(|o| o.get("emailAddress"))
                    .and_then(|e| e.as_str())
                    .map(str::to_string);
                let blob = if is_active {
                    be.live_blob()
                } else {
                    refresh_with_backoff(be, st, &slot, now_ts)
                };
                (email, blob)
            }
        };
        let email = email.as_deref();

        let row = match blob {
            Err(e) => usage::error_row(&slot, is_active, email, &e),
            Ok(blob) => match blob
                .get("claudeAiOauth")
                .and_then(|o| o.get("accessToken"))
                .and_then(|t| t.as_str())
            {
                None => usage::error_row(&slot, is_active, email, "KeyError: 'accessToken'"),
                Some(token) => match be.usage(token) {
                    Ok(u) => usage::account_row(&slot, is_active, email, &u),
                    Err(f) => usage::error_row(&slot, is_active, email, &f.message),
                },
            },
        };
        rows.push(row);
    }

    usage::cache_document(now_ts, active.as_deref(), rows)
}

// --- the whole poll run -----------------------------------------------------

/// The files a poll run touches.
pub struct Paths {
    pub cache: PathBuf,
    pub state: PathBuf,
    pub switch_log: PathBuf,
    pub auto_flag: PathBuf,
}

impl Paths {
    pub fn live() -> Paths {
        let d = slots::config_dir();
        Paths {
            cache: d.join("usage.json"),
            state: d.join("state.json"),
            switch_log: d.join("switches.jsonl"),
            auto_flag: d.join("auto.flag"),
        }
    }
}

/// Write the usage cache atomically. Everything reads this file — the widget every five
/// seconds, the dashboard, the CLI, the switcher — so a torn write is a cache that reads
/// as "no accounts configured", which is what a healthy empty machine looks like.
pub fn write_cache_at(path: &Path, cache: &Value) -> Result<(), String> {
    let dir = path.parent().ok_or("cache path has no parent")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join(format!(".usage.{}.tmp", std::process::id()));
    std::fs::write(&tmp, cache.to_string()).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// A macOS notification. Best-effort by design — a failed toast must not fail a poll.
pub fn notify(title: &str, message: &str) {
    let script = format!(
        "display notification {} with title {}",
        serde_json::Value::String(message.into()),
        serde_json::Value::String(title.into())
    );
    let _ = std::process::Command::new("osascript").args(["-e", &script]).output();
}

/// Render one switch-log line the way `json.dumps(..., sort_keys=True)` does.
///
/// Keys sorted, and Python's default separators — `", "` and `": "` — because this file is
/// append-only and has Python-written lines already in it. Matching keeps one file in one
/// format; nothing parses it that would care either way (only `json.loads` readers).
pub fn switch_log_line(cache: &Value, from: Option<&str>, to: &str, reason: &str, now_ts: i64)
    -> String
{
    let s = |v: &str| serde_json::Value::String(v.into()).to_string();
    let num = |v: Option<f64>| match v {
        Some(n) => serde_json::json!(n).to_string(),
        None => "null".to_string(),
    };
    let util_of = |row: &Value, key: &str| -> Option<f64> {
        row.get(key)?.get("util")?.as_f64()
    };

    let mut rows: Vec<(String, String)> = cache
        .get("accounts")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    let slot = r.get("slot")?.as_str()?.to_string();
                    // "fiveHour" sorts before "sevenDay", so this is already sorted.
                    let body = format!(
                        "{{\"fiveHour\": {}, \"sevenDay\": {}}}",
                        num(util_of(r, "fiveHour")),
                        num(util_of(r, "sevenDay"))
                    );
                    Some((slot, body))
                })
                .collect()
        })
        .unwrap_or_default();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let util = rows
        .iter()
        .map(|(k, v)| format!("{}: {}", s(k), v))
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        "{{\"from\": {}, \"reason\": {}, \"to\": {}, \"ts\": {}, \"util\": {{{}}}}}",
        match from {
            Some(f) => s(f),
            None => "null".into(),
        },
        s(reason),
        s(to),
        now_ts,
        util
    )
}

/// Append one self-contained record of a switch.
///
/// **Never fails the caller.** A poll that dies because its own audit log is unwritable is
/// strictly worse than an unlogged switch. Before this existed a switch left only a macOS
/// toast, which is gone the moment you look away — and a switch INTO an exhausted account
/// went unnoticed because answering "did it fire, and why" meant inferring it from one
/// `last_switch_ts` and an hourly usage sample.
pub fn log_switch(path: &Path, cache: &Value, from: Option<&str>, to: &str, reason: &str,
                  now_ts: i64) {
    use std::io::Write;
    let line = switch_log_line(cache, from, to, reason, now_ts);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

/// Is the auto-switcher enabled? The flag is the file's existence, nothing else.
pub fn auto_enabled(flag: &Path) -> bool {
    flag.exists()
}

/// Turn the auto-switcher on or off by creating or removing the flag file.
pub fn set_auto(flag: &Path, on: bool) -> Result<(), String> {
    if on {
        if let Some(d) = flag.parent() {
            std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
        }
        std::fs::write(flag, "").map_err(|e| format!("{}: {e}", flag.display()))
    } else if flag.exists() {
        std::fs::remove_file(flag).map_err(|e| format!("{}: {e}", flag.display()))
    } else {
        Ok(())
    }
}

/// One full poll: refresh, fetch, write the cache, and switch if the policy says so.
///
/// The cache is written FIRST and unconditionally, before any switching. Everything on
/// this machine reads that file, and a poll that switched but never published what it saw
/// leaves every reader on data from three minutes ago while the active account has
/// changed underneath them.
#[allow(clippy::too_many_arguments)]
pub fn run_poll(
    be: &dyn Backend,
    vault: &dyn crate::switch::Vault,
    paths: &Paths,
    cfg: &crate::accounts::SwitchConfig,
    now_ts: i64,
    notifier: &dyn Fn(&str, &str),
) -> Value {
    let mut st = load_state_at(&paths.state);
    let cache = poll_all(be, &mut st, now_ts);
    // Persist the backoff BEFORE anything that can fail. It is the only record that a
    // slot is rate-limited, and losing it means the next poll hammers the same endpoint.
    let _ = save_state_at(&paths.state, &st);

    if let Err(e) = write_cache_at(&paths.cache, &cache) {
        // Publish the failure rather than leaving a stale cache to be read as current.
        let err = serde_json::json!({
            "polledAt": now_ts, "active": Value::Null, "accounts": [], "error": e,
        });
        let _ = write_cache_at(&paths.cache, &err);
        return err;
    }

    if !auto_enabled(&paths.auto_flag) {
        return cache;
    }

    let Some(accounts) = crate::accounts::parse(&cache.to_string()) else {
        return cache;
    };
    let last = st.get("last_switch_ts").and_then(|x| x.as_i64()).unwrap_or(0);
    let (Some(target), reason) = crate::accounts::decide_switch(&accounts, cfg, last, now_ts)
    else {
        return cache;
    };

    let from = vault.current_slot();
    match crate::switch::use_slot(vault, &target, Some(&|slot: &str, blob: &Value| {
        refresh_for_switch(be, slot, blob, now_ts)
    })) {
        Ok(()) => {
            st["last_switch_ts"] = serde_json::json!(now_ts);
            let _ = save_state_at(&paths.state, &st);
            log_switch(&paths.switch_log, &cache, from.as_deref(), &target, &reason, now_ts);
            notifier(
                "Claude account switched",
                &format!("{} → {} ({})", from.as_deref().unwrap_or("none"), target, reason),
            );
        }
        Err(e) => notifier("Claude auto-switch failed", &e.to_string()),
    }
    cache
}

/// Refresh a slot's credential on the way into a switch, if it needs it.
///
/// No backoff here on purpose: this is a deliberate switch that is already happening, and
/// declining it because of a rate-limit window recorded by the POLLER would abandon the
/// switch after the picker already chose. The poll path owns the backoff.
fn refresh_for_switch(be: &dyn Backend, slot: &str, blob: &Value, now_ts: i64)
    -> Result<Value, String>
{
    let now_ms = now_ts * 1000;
    if !usage::needs_refresh(blob, now_ms) {
        return Ok(blob.clone());
    }
    let Some(rt) = blob.get("claudeAiOauth").and_then(|o| o.get("refreshToken"))
        .and_then(|t| t.as_str()) else { return Ok(blob.clone()) };
    let resp = be.refresh(rt).map_err(|f| f.message)?;
    let merged = usage::merge_refreshed(blob, &resp, now_ms);
    be.store_blob(slot, &merged)?;
    Ok(merged)
}

/// Switch to `slot` for real, refreshing its credential on the way in.
///
/// ONE switcher, for the same reason there is one picker. The CLI, the Telegram `/switch`
/// and the auto-poller all land here, so the re-stash of the outgoing account, the
/// rollback on a failed identity write and the read-only-keychain refusal cannot be
/// present in one path and missing from another. Two of these used to shell
/// `claude-acct use` instead, which meant the rules lived in Python for some callers and
/// in Rust for others.
pub fn switch_now(slot: &str, now_ts: i64) -> Result<(), crate::switch::SwitchError> {
    let be = Live;
    crate::switch::use_slot(&crate::switch::Live, slot, Some(&|s: &str, b: &Value| {
        refresh_for_switch(&be, s, b, now_ts)
    }))
}

/// Unix seconds now.
pub fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The real backend: keychain, `~/.claude.json`, and the network.
pub struct Live;

impl Backend for Live {
    fn slots(&self) -> Vec<String> {
        slots::list_slots()
    }
    fn active(&self) -> Option<String> {
        slots::current_slot()
    }
    fn identity(&self, slot: &str) -> Result<Value, String> {
        slots::load_slot_identity(slot).ok_or_else(|| format!("no identity stashed for '{slot}'"))
    }
    fn live_blob(&self) -> Result<Value, String> {
        slots::read_live_blob()
    }
    fn slot_blob(&self, slot: &str) -> Result<Value, String> {
        slots::load_slot_blob(slot)
    }
    fn store_blob(&self, slot: &str, blob: &Value) -> Result<(), String> {
        slots::store_slot_blob(slot, blob)
    }
    fn refresh(&self, refresh_token: &str) -> Result<Value, HttpFail> {
        usage::oauth_refresh(refresh_token, &usage::user_agent())
    }
    fn usage(&self, access_token: &str) -> Result<Value, HttpFail> {
        usage::fetch_usage(access_token, &usage::user_agent())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A scripted backend. Records the calls, so the tests can assert on what the loop
    /// did NOT do — "the active account was never refreshed" is only checkable that way.
    #[derive(Default)]
    struct Fake {
        slots: Vec<String>,
        active: Option<String>,
        /// slot -> identity JSON, or Err text
        identity: std::collections::BTreeMap<String, Result<Value, String>>,
        live: Option<Result<Value, String>>,
        blobs: std::collections::BTreeMap<String, Result<Value, String>>,
        refresh_result: Option<Result<Value, HttpFail>>,
        usage_result: Option<Result<Value, HttpFail>>,
        store_fails: bool,
        calls: RefCell<Vec<String>>,
        stored: RefCell<Vec<(String, Value)>>,
    }

    fn blob(expires_at: i64, rt: Option<&str>, token: &str) -> Value {
        let mut o = serde_json::json!({ "accessToken": token, "expiresAt": expires_at });
        if let Some(rt) = rt {
            o["refreshToken"] = serde_json::json!(rt);
        }
        serde_json::json!({ "claudeAiOauth": o })
    }

    fn ident(email: &str) -> Value {
        serde_json::json!({ "oauthAccount": { "emailAddress": email } })
    }

    fn usage_doc(five: f64, seven: f64) -> Value {
        serde_json::json!({
            "five_hour":  { "utilization": five,  "resets_at": "2026-08-01T00:00:00Z" },
            "seven_day":  { "utilization": seven, "resets_at": "2026-08-05T00:00:00Z" },
        })
    }

    impl Backend for Fake {
        fn slots(&self) -> Vec<String> {
            self.slots.clone()
        }
        fn active(&self) -> Option<String> {
            self.active.clone()
        }
        fn identity(&self, slot: &str) -> Result<Value, String> {
            self.calls.borrow_mut().push(format!("identity:{slot}"));
            self.identity.get(slot).cloned().unwrap_or_else(|| Ok(ident("x@y.z")))
        }
        fn live_blob(&self) -> Result<Value, String> {
            self.calls.borrow_mut().push("live_blob".into());
            self.live.clone().unwrap_or_else(|| Ok(blob(0, None, "LIVE")))
        }
        fn slot_blob(&self, slot: &str) -> Result<Value, String> {
            self.calls.borrow_mut().push(format!("slot_blob:{slot}"));
            self.blobs.get(slot).cloned().unwrap_or_else(|| Ok(blob(0, Some("rt"), "OLD")))
        }
        fn store_blob(&self, slot: &str, b: &Value) -> Result<(), String> {
            self.calls.borrow_mut().push(format!("store:{slot}"));
            if self.store_fails {
                return Err("RuntimeError: keychain write failed for x (rc=161)".into());
            }
            self.stored.borrow_mut().push((slot.into(), b.clone()));
            Ok(())
        }
        fn refresh(&self, _rt: &str) -> Result<Value, HttpFail> {
            self.calls.borrow_mut().push("refresh".into());
            self.refresh_result
                .clone()
                .unwrap_or_else(|| Ok(serde_json::json!({ "access_token": "NEW", "expires_in": 3600 })))
        }
        fn usage(&self, token: &str) -> Result<Value, HttpFail> {
            self.calls.borrow_mut().push(format!("usage:{token}"));
            self.usage_result.clone().unwrap_or_else(|| Ok(usage_doc(10.0, 20.0)))
        }
    }

    fn util(row: &Value, key: &str) -> Option<f64> {
        row.get(key)?.get("util")?.as_f64()
    }

    #[test]
    fn the_active_account_is_polled_from_the_live_credential_and_never_refreshed() {
        // Claude Code rotates the live refresh token as it runs. Refreshing it from here
        // races the CLI and can invalidate the token the CLI is holding — so the
        // assertion that matters is the NEGATIVE one, that `refresh` was never called.
        let f = Fake {
            slots: vec!["a".into(), "b".into()],
            active: Some("a".into()),
            ..Default::default()
        };
        let mut st = serde_json::json!({});
        let cache = poll_all(&f, &mut st, 1_000_000);

        let calls = f.calls.borrow().clone();
        assert!(calls.contains(&"live_blob".to_string()), "{calls:?}");
        assert!(!calls.contains(&"slot_blob:a".to_string()),
                "the active slot's STASHED blob must not be read: {calls:?}");
        assert!(calls.contains(&"usage:LIVE".to_string()),
                "the live token is what gets sent: {calls:?}");
        // 'b' is not active, so it does refresh.
        assert!(calls.contains(&"slot_blob:b".to_string()), "{calls:?}");
        assert_eq!(cache["active"], serde_json::json!("a"));
        assert_eq!(cache["accounts"][0]["active"], serde_json::json!(true));
        assert_eq!(cache["accounts"][1]["active"], serde_json::json!(false));
    }

    #[test]
    fn one_broken_account_does_not_blank_the_cache_for_the_others() {
        // The failure this guards: an empty `accounts` array is what a machine with NO
        // accounts looks like, and that is the shape the switcher reads. A cache that
        // empties itself because one token expired reads as "nothing to switch to".
        let mut identity = std::collections::BTreeMap::new();
        identity.insert("broken".to_string(), Err("no identity stashed for 'broken'".to_string()));
        let f = Fake {
            slots: vec!["broken".into(), "fine".into()],
            active: Some("fine".into()),
            identity,
            ..Default::default()
        };
        let mut st = serde_json::json!({});
        let cache = poll_all(&f, &mut st, 1_000_000);

        let rows = cache["accounts"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "both accounts appear");
        assert!(rows[0]["error"].as_str().unwrap().contains("no identity"));
        assert!(rows[0].get("fiveHour").is_none(), "an errored row carries no windows");
        assert_eq!(util(&rows[1], "fiveHour"), Some(10.0), "the healthy account still polled");
    }

    #[test]
    fn an_http_error_becomes_an_errored_row_rather_than_an_idle_looking_one() {
        // curl exits 0 on a 401 and its body is valid JSON, so a status-blind fetch
        // produces a row with null windows and NO error — which reads as an account that
        // has used nothing. Measured: `curl -sS` against a 401 returns exit 0.
        let f = Fake {
            slots: vec!["a".into()],
            active: Some("a".into()),
            usage_result: Some(Err(HttpFail {
                status: Some(401),
                message: "HTTPError: HTTP Error 401: {\"error\":\"invalid_token\"}".into(),
            })),
            ..Default::default()
        };
        let mut st = serde_json::json!({});
        let cache = poll_all(&f, &mut st, 1_000_000);
        let row = &cache["accounts"][0];
        assert!(row["error"].as_str().unwrap().contains("401"));
        assert!(row.get("fiveHour").is_none(),
                "no window at all — a null `util` would look like an unused account");
    }

    #[test]
    fn a_429_arms_a_backoff_and_the_next_poll_does_not_call_the_endpoint_again() {
        let f = Fake {
            slots: vec!["a".into(), "b".into()],
            active: Some("a".into()),
            refresh_result: Some(Err(HttpFail {
                status: Some(429),
                message: "HTTPError: HTTP Error 429: rate limited".into(),
            })),
            ..Default::default()
        };
        let mut st = serde_json::json!({});
        let cache = poll_all(&f, &mut st, 1_000_000);
        assert!(cache["accounts"][1]["error"].as_str().unwrap().contains("429"));
        assert_eq!(recorded_backoff(&st, "b"), Some((1_000_000 + 600, 1)),
                   "first 429 waits REFRESH_BACKOFF_BASE");

        // Second poll, still inside the window: the endpoint must NOT be called again.
        // Without this the poller re-attempts every StartInterval and the account never
        // recovers — which is the entire reason the backoff is persisted.
        f.calls.borrow_mut().clear();
        let cache = poll_all(&f, &mut st, 1_000_100);
        assert!(!f.calls.borrow().contains(&"refresh".to_string()),
                "still rate-limited: {:?}", f.calls.borrow());
        assert!(cache["accounts"][1]["error"].as_str().unwrap().contains("rate-limited"));
        assert_eq!(recorded_backoff(&st, "b").map(|(_, n)| n), Some(1),
                   "a skipped attempt is not a new failure");

        // Past the window it tries again, and a second 429 doubles the wait.
        let mut st2 = st.clone();
        poll_all(&f, &mut st2, 1_000_700);
        assert_eq!(recorded_backoff(&st2, "b"), Some((1_000_700 + 1200, 2)));
    }

    #[test]
    fn a_slot_whose_token_is_still_valid_clears_its_backoff_without_calling_refresh() {
        // A hand re-login leaves a good token behind. Leaving the backoff armed would
        // refuse to poll a working account for up to an hour, and the backoff describes
        // the refresh ENDPOINT, not the account.
        let mut blobs = std::collections::BTreeMap::new();
        // expiresAt well beyond now + the 60s margin
        blobs.insert("b".to_string(), Ok(blob(2_000_000_000, Some("rt"), "FRESH")));
        let f = Fake {
            slots: vec!["b".into()],
            active: None,
            blobs,
            ..Default::default()
        };
        let mut st = serde_json::json!({});
        set_backoff(&mut st, "b", 1_000_600, 3);
        let cache = poll_all(&f, &mut st, 1_000_000);

        assert!(!f.calls.borrow().contains(&"refresh".to_string()));
        assert_eq!(recorded_backoff(&st, "b"), None, "the backoff is forgotten");
        assert_eq!(util(&cache["accounts"][0], "fiveHour"), Some(10.0));
    }

    #[test]
    fn a_refresh_that_cannot_be_stashed_reports_the_write_failure() {
        // rc=161 is the sandbox refusing the keychain write. The merged blob holds a
        // possibly-rotated refresh token; losing it silently bricks the slot at its next
        // refresh, so the write failure must reach the row.
        let f = Fake {
            slots: vec!["b".into()],
            active: None,
            store_fails: true,
            ..Default::default()
        };
        let mut st = serde_json::json!({});
        let cache = poll_all(&f, &mut st, 1_000_000);
        let err = cache["accounts"][0]["error"].as_str().unwrap();
        assert!(err.contains("keychain write failed"), "{err}");
        assert!(err.contains("161"), "the rc identifies the sandbox: {err}");
    }

    #[test]
    fn a_successful_refresh_stashes_the_merged_blob_and_polls_with_the_new_token() {
        let f = Fake {
            slots: vec!["b".into()],
            active: None,
            ..Default::default()
        };
        let mut st = serde_json::json!({});
        poll_all(&f, &mut st, 1_000_000);
        let stored = f.stored.borrow().clone();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].1["claudeAiOauth"]["accessToken"], serde_json::json!("NEW"));
        assert_eq!(stored[0].1["claudeAiOauth"]["refreshToken"], serde_json::json!("rt"),
                   "a response without refresh_token keeps the old one");
        assert!(f.calls.borrow().contains(&"usage:NEW".to_string()),
                "the FRESH token is what gets sent: {:?}", f.calls.borrow());
    }

    #[test]
    fn the_email_survives_onto_an_errored_row() {
        // The dashboard names accounts by email. An error row that lost it says only
        // "some slot is broken", which is not actionable.
        let f = Fake {
            slots: vec!["b".into()],
            active: None,
            usage_result: Some(Err(HttpFail { status: Some(500), message: "boom".into() })),
            ..Default::default()
        };
        let mut st = serde_json::json!({});
        let cache = poll_all(&f, &mut st, 1_000_000);
        assert_eq!(cache["accounts"][0]["email"], serde_json::json!("x@y.z"));
    }

    #[test]
    fn no_accounts_yields_an_empty_cache_rather_than_a_missing_key() {
        let f = Fake::default();
        let mut st = serde_json::json!({});
        let cache = poll_all(&f, &mut st, 42);
        assert_eq!(cache["polledAt"], serde_json::json!(42));
        assert_eq!(cache["active"], Value::Null);
        assert_eq!(cache["accounts"], serde_json::json!([]),
                   "an array, so readers can iterate without checking for the key");
    }

    #[test]
    fn state_round_trips_and_keeps_keys_the_poller_does_not_own() {
        // The poller writes `refresh_backoff`; the SWITCHER writes `last_switch_ts` into
        // the same file. A typed struct would silently drop the other's key on write, and
        // losing `last_switch_ts` disables the switch cooldown.
        let dir = std::env::temp_dir().join(format!("paos-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("state.json");

        let mut st = serde_json::json!({ "last_switch_ts": 1234, "unknown_future_key": [1, 2] });
        set_backoff(&mut st, "b", 999, 2);
        save_state_at(&p, &st).unwrap();

        let back = load_state_at(&p);
        assert_eq!(back["last_switch_ts"], serde_json::json!(1234));
        assert_eq!(back["unknown_future_key"], serde_json::json!([1, 2]));
        assert_eq!(recorded_backoff(&back, "b"), Some((999, 2)));

        let mut back2 = back.clone();
        clear_backoff(&mut back2, "b");
        assert_eq!(recorded_backoff(&back2, "b"), None);
        assert_eq!(back2["last_switch_ts"], serde_json::json!(1234), "clearing kept the rest");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_state_file_does_not_stop_a_poll() {
        let dir = std::env::temp_dir().join(format!("paos-state-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("state.json");
        std::fs::write(&p, "{not json").unwrap();
        assert_eq!(load_state_at(&p), serde_json::json!({}));
        // A JSON scalar is not a state object either.
        std::fs::write(&p, "7").unwrap();
        assert_eq!(load_state_at(&p), serde_json::json!({}));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- run_poll ----------------------------------------------------------

    #[derive(Default)]
    struct FakeVault {
        writable: bool,
        current: Option<String>,
        fail: bool,
        switched: RefCell<Vec<String>>,
    }

    impl crate::switch::Vault for FakeVault {
        fn writable(&self) -> bool { self.writable }
        fn slot_blob(&self, _s: &str) -> Result<Value, String> { Ok(blob(2_000_000_000, None, "T")) }
        fn slot_identity(&self, _s: &str) -> Result<Value, String> {
            Ok(serde_json::json!({ "userID": "u", "oauthAccount": { "accountUuid": "x" } }))
        }
        fn live_blob(&self) -> Result<Value, String> { Ok(blob(2_000_000_000, None, "L")) }
        fn write_live(&self, _b: &Value) -> Result<(), String> {
            if self.fail { Err("keychain write failed (rc=161)".into()) } else { Ok(()) }
        }
        fn write_slot(&self, _s: &str, _b: &Value, _i: &Value) -> Result<(), String> { Ok(()) }
        fn read_claude_json(&self) -> Result<Value, String> { Ok(serde_json::json!({})) }
        fn write_claude_json(&self, _v: &Value) -> Result<(), String> {
            self.switched.borrow_mut().push("cj".into());
            Ok(())
        }
        fn current_slot(&self) -> Option<String> { self.current.clone() }
    }

    fn tmp(tag: &str) -> Paths {
        let d = std::env::temp_dir()
            .join(format!("paos-run-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Paths {
            cache: d.join("usage.json"),
            state: d.join("state.json"),
            switch_log: d.join("switches.jsonl"),
            auto_flag: d.join("auto.flag"),
        }
    }

    /// A backend whose ACTIVE account is over the burst ceiling and whose other account
    /// has room — i.e. the picker will want to switch.
    fn tripping() -> Fake {
        Fake {
            slots: vec!["a".into(), "b".into()],
            active: Some("a".into()),
            usage_result: None,
            ..Default::default()
        }
    }

    #[test]
    fn the_switch_log_line_matches_the_one_the_python_actually_wrote() {
        // Copied from ~/.config/claude-usage/switches.jsonl, not reconstructed from
        // memory of what json.dumps does: sorted keys, `", "` and `": "` separators,
        // floats keeping their `.0`. The file is append-only and already holds
        // Python-written lines, so a drift here leaves one file in two formats.
        let cache = serde_json::json!({
            "accounts": [
                { "slot": "third_example.com", "fiveHour": {"util": 95.0}, "sevenDay": {"util": 45.0} },
                { "slot": "first_example.com",     "fiveHour": {"util": 13.0}, "sevenDay": {"util": 100.0} },
                { "slot": "second_example.com",     "fiveHour": {"util": 0.0},  "sevenDay": {"util": 15.0} },
            ]
        });
        let got = switch_log_line(&cache, Some("third_example.com"), "second_example.com",
                                  "active 5h 95.0% >= 95", 1785505329);
        assert_eq!(got, "{\"from\": \"third_example.com\", \"reason\": \"active 5h 95.0% >= 95\", \"to\": \"second_example.com\", \"ts\": 1785505329, \"util\": {\"first_example.com\": {\"fiveHour\": 13.0, \"sevenDay\": 100.0}, \"second_example.com\": {\"fiveHour\": 0.0, \"sevenDay\": 15.0}, \"third_example.com\": {\"fiveHour\": 95.0, \"sevenDay\": 45.0}}}");
    }

    #[test]
    fn an_unpollable_account_logs_null_utils_rather_than_zeroes() {
        let cache = serde_json::json!({
            "accounts": [ { "slot": "dead", "error": "HTTPError: HTTP Error 401" } ]
        });
        let got = switch_log_line(&cache, None, "b", "why", 7);
        assert!(got.contains("\"dead\": {\"fiveHour\": null, \"sevenDay\": null}"), "{got}");
        assert!(got.starts_with("{\"from\": null,"), "an unknown outgoing slot is null: {got}");
    }

    #[test]
    fn the_cache_is_written_before_any_switching_is_considered() {
        // Everything on this machine reads that file. A poll that switched but never
        // published what it saw leaves every reader on stale data while the active
        // account changed underneath them.
        let p = tmp("cachefirst");
        let be = tripping();
        let v = FakeVault { writable: true, current: Some("a".into()), ..Default::default() };
        // auto.flag absent — no switching at all, but the cache must still land.
        let cache = run_poll(&be, &v, &p, &crate::accounts::SwitchConfig::default(), 1_000_000,
                             &|_, _| {});
        let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(&p.cache).unwrap()).unwrap();
        assert_eq!(on_disk, cache);
        assert_eq!(on_disk["accounts"].as_array().unwrap().len(), 2);
        assert!(v.switched.borrow().is_empty(), "auto is off: no switch");
    }

    #[test]
    fn the_backoff_is_persisted_even_when_nothing_else_happens() {
        // It is the only record that a slot is rate-limited. Losing it means the next
        // poll hammers the same endpoint, which is the failure the backoff exists for.
        let p = tmp("backoff");
        let mut be = tripping();
        be.refresh_result = Some(Err(HttpFail { status: Some(429), message: "429".into() }));
        let v = FakeVault { writable: true, ..Default::default() };
        run_poll(&be, &v, &p, &crate::accounts::SwitchConfig::default(), 1_000_000, &|_, _| {});
        let st = load_state_at(&p.state);
        assert_eq!(recorded_backoff(&st, "b"), Some((1_000_600, 1)));
    }

    #[test]
    fn with_auto_enabled_a_trip_switches_and_leaves_an_audit_trail() {
        let p = tmp("switch");
        std::fs::write(&p.auto_flag, "").unwrap();
        let mut be = tripping();
        // active 'a' at 99% burst, 'b' with room.
        be.usage_result = None;
        let mut ident = std::collections::BTreeMap::new();
        ident.insert("a".to_string(), Ok(ident_email("a@x")));
        ident.insert("b".to_string(), Ok(ident_email("b@x")));
        be.identity = ident;
        // Give 'a' a hot 5-hour window and 'b' a cool one via per-token usage.
        let be = HotActive { inner: be };

        let v = FakeVault { writable: true, current: Some("a".into()), ..Default::default() };
        let notes = RefCell::new(vec![]);
        run_poll(&be, &v, &p, &crate::accounts::SwitchConfig::default(), 1_000_000,
                 &|t: &str, m: &str| notes.borrow_mut().push(format!("{t}|{m}")));

        assert_eq!(v.switched.borrow().len(), 1, "the switch happened");
        let st = load_state_at(&p.state);
        assert_eq!(st["last_switch_ts"], serde_json::json!(1_000_000),
                   "the cooldown clock starts, or the next poll switches again immediately");
        let log = std::fs::read_to_string(&p.switch_log).unwrap();
        assert!(log.trim_end().ends_with('}') && log.ends_with('\n'), "one line: {log:?}");
        assert!(log.contains("\"to\": \"b\""), "{log}");
        assert_eq!(notes.borrow().len(), 1);
        assert!(notes.borrow()[0].starts_with("Claude account switched|a → b"), "{:?}", notes.borrow());
    }

    #[test]
    fn a_failed_switch_notifies_and_does_not_start_the_cooldown() {
        // Recording last_switch_ts for a switch that did not happen would make the next
        // poll decline for the whole cooldown while the account is still exhausted.
        let p = tmp("failswitch");
        std::fs::write(&p.auto_flag, "").unwrap();
        let mut be = tripping();
        let mut ident = std::collections::BTreeMap::new();
        ident.insert("a".to_string(), Ok(ident_email("a@x")));
        ident.insert("b".to_string(), Ok(ident_email("b@x")));
        be.identity = ident;
        let be = HotActive { inner: be };
        let v = FakeVault { writable: true, current: Some("a".into()), fail: true, ..Default::default() };
        let notes = RefCell::new(vec![]);
        run_poll(&be, &v, &p, &crate::accounts::SwitchConfig::default(), 1_000_000,
                 &|t: &str, m: &str| notes.borrow_mut().push(format!("{t}|{m}")));

        let st = load_state_at(&p.state);
        assert!(st.get("last_switch_ts").is_none(), "no cooldown started: {st}");
        assert!(!p.switch_log.exists(), "nothing to audit");
        assert!(notes.borrow()[0].starts_with("Claude auto-switch failed|"), "{:?}", notes.borrow());
    }

    #[test]
    fn a_read_only_keychain_reports_the_failure_instead_of_switching() {
        // The sandbox case reaching run_poll: it must say so, not fail silently.
        let p = tmp("ro");
        std::fs::write(&p.auto_flag, "").unwrap();
        let mut be = tripping();
        let mut ident = std::collections::BTreeMap::new();
        ident.insert("a".to_string(), Ok(ident_email("a@x")));
        ident.insert("b".to_string(), Ok(ident_email("b@x")));
        be.identity = ident;
        let be = HotActive { inner: be };
        let v = FakeVault { writable: false, current: Some("a".into()), ..Default::default() };
        let notes = RefCell::new(vec![]);
        run_poll(&be, &v, &p, &crate::accounts::SwitchConfig::default(), 1_000_000,
                 &|t: &str, m: &str| notes.borrow_mut().push(format!("{t}|{m}")));
        assert!(notes.borrow()[0].contains("terminal"), "{:?}", notes.borrow());
        assert!(v.switched.borrow().is_empty());
    }

    fn ident_email(e: &str) -> Value {
        serde_json::json!({ "oauthAccount": { "emailAddress": e } })
    }

    /// Wraps a `Fake` so the ACTIVE account's token reports a hot 5-hour window and every
    /// other token reports a cool one — enough to make the picker want to move.
    struct HotActive {
        inner: Fake,
    }

    impl Backend for HotActive {
        fn slots(&self) -> Vec<String> { self.inner.slots() }
        fn active(&self) -> Option<String> { self.inner.active() }
        fn identity(&self, s: &str) -> Result<Value, String> { self.inner.identity(s) }
        fn live_blob(&self) -> Result<Value, String> { self.inner.live_blob() }
        fn slot_blob(&self, s: &str) -> Result<Value, String> { self.inner.slot_blob(s) }
        fn store_blob(&self, s: &str, b: &Value) -> Result<(), String> { self.inner.store_blob(s, b) }
        fn refresh(&self, rt: &str) -> Result<Value, HttpFail> { self.inner.refresh(rt) }
        fn usage(&self, token: &str) -> Result<Value, HttpFail> {
            // "LIVE" is the active account's token in `Fake::live_blob`.
            Ok(if token == "LIVE" { usage_doc(99.0, 40.0) } else { usage_doc(10.0, 20.0) })
        }
    }

    #[test]
    fn a_healthy_poll_does_not_grow_an_empty_backoff_key() {
        let f = Fake { slots: vec!["b".into()], active: None, ..Default::default() };
        let mut st = serde_json::json!({ "last_switch_ts": 5 });
        poll_all(&f, &mut st, 1_000_000);
        assert_eq!(st, serde_json::json!({ "last_switch_ts": 5 }),
                   "nothing to record means nothing written");
    }
}
