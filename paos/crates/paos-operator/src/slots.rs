//! Claude account slots: the credentials and identity layer under `paos accounts`.
//!
//! Ported from `claude_accounts.py`. This module is the READ half — which slots exist,
//! which one is live, and what identity each carries. It deliberately contains no writer.
//!
//! **The keychain is read-yes / write-no from inside an agent sandbox**, and that is not
//! a limitation to route around. Measured on this machine: `security
//! find-generic-password -w` returns rc=0 from a session, while the write path returns
//! rc=161 (`errSecInteractionNotAllowed`). So a session can see which account is live and
//! cannot change it — which is correct, because switching accounts is a machine-global
//! act like a deploy. The write belongs in a terminal or the LaunchAgent, and the
//! session-side path must refuse loudly rather than appear to work: the Python died with
//! a `RuntimeError` traceback instead, which is how a failed switch looked like a crash.

use std::path::PathBuf;
use std::process::Command;

/// The keychain service holding the LIVE credentials.
pub const LIVE_SERVICE: &str = "Claude Code-credentials";

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

/// `~/.config/claude-usage`. `PAOS_ACCOUNTS_DIR` overrides the whole tree for tests.
pub fn config_dir() -> PathBuf {
    if let Ok(d) = std::env::var("PAOS_ACCOUNTS_DIR") {
        if !d.trim().is_empty() {
            return PathBuf::from(d);
        }
    }
    home().join(".config/claude-usage")
}

pub fn accounts_dir() -> PathBuf {
    config_dir().join("accounts")
}

/// The per-slot keychain service. One item per slot, alongside the live one.
pub fn slot_service(slot: &str) -> String {
    format!("{LIVE_SERVICE}-{slot}")
}

/// A filesystem- and service-safe slot name derived from an email.
///
/// Mirrors the Python exactly: lowercase, anything outside `[a-z0-9._-]` collapses to a
/// single `_`, strip leading/trailing `_`, and an empty result becomes `account`. It
/// names a keychain item and a file, so a drift here orphans an existing slot rather than
/// erroring.
pub fn slot_from_email(email: &str) -> String {
    let lower = email.trim().to_lowercase();
    let mut out = String::new();
    let mut last_us = false;
    for c in lower.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-' {
            out.push(c);
            last_us = false;
        } else if !last_us {
            out.push('_');
            last_us = true;
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() { "account".to_string() } else { trimmed.to_string() }
}

/// Read a keychain secret. `None` when absent OR unreadable — the caller cannot tell the
/// difference and must not pretend to.
pub fn keychain_read(service: &str) -> Option<String> {
    let user = std::env::var("USER").unwrap_or_default();
    let out = Command::new("security")
        .args(["find-generic-password", "-s", service, "-a", &user, "-w"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Write a keychain secret.
///
/// `-U` updates in place when the item exists. The rc is carried into the message because
/// **161 is `errSecInteractionNotAllowed`** — the sandbox refusing, not a broken keychain —
/// and telling those apart from the error text is the difference between "re-login" and
/// "run this outside the agent".
pub fn keychain_write(service: &str, secret: &str) -> Result<(), String> {
    let user = std::env::var("USER").unwrap_or_default();
    let out = Command::new("security")
        .args(["add-generic-password", "-U", "-a", &user, "-s", service, "-w", secret])
        .output()
        .map_err(|e| format!("security: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let rc = out.status.code().unwrap_or(-1);
    Err(format!("RuntimeError: keychain write failed for {service} (rc={rc})"))
}

/// The LIVE credential blob — the account Claude Code is signed in as right now.
pub fn read_live_blob() -> Result<serde_json::Value, String> {
    let raw = keychain_read(LIVE_SERVICE)
        .ok_or_else(|| format!("NoCredentials: no Keychain item '{LIVE_SERVICE}'"))?;
    serde_json::from_str(&raw).map_err(|e| format!("credentials were not JSON: {e}"))
}

/// A slot's stashed credential blob.
pub fn load_slot_blob(slot: &str) -> Result<serde_json::Value, String> {
    let raw = keychain_read(&slot_service(slot))
        .ok_or_else(|| format!("NoCredentials: no stashed creds for slot '{slot}'"))?;
    serde_json::from_str(&raw).map_err(|e| format!("stashed creds were not JSON: {e}"))
}

/// Stash a credential blob back into a slot after a refresh.
pub fn store_slot_blob(slot: &str, blob: &serde_json::Value) -> Result<(), String> {
    keychain_write(&slot_service(slot), &blob.to_string())
}

/// Slots that exist, sorted. A missing directory is "none", not an error: a machine that
/// has never captured an account is a normal first-run state.
pub fn list_slots() -> Vec<String> {
    list_slots_at(&accounts_dir())
}

/// Split so tests need not set PAOS_ACCOUNTS_DIR. Env vars are process-global and Rust
/// runs tests as threads, so mutating one races every other test that reads it — the
/// defect that let a suite write the live fleet store earlier in this migration.
pub fn list_slots_at(dir: &std::path::Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".json")).map(str::to_string))
        .collect();
    v.sort();
    v
}

/// The identity JSON stashed for a slot.
pub fn load_slot_identity(slot: &str) -> Option<serde_json::Value> {
    let p = accounts_dir().join(format!("{slot}.json"));
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

/// Stash a credential blob and its identity into a slot.
///
/// The keychain first: an identity file with no matching credential describes an account
/// that cannot be switched to, and `list_slots` enumerates those files — so writing the
/// file first would advertise a slot that does not work.
pub fn save_slot(slot: &str, blob: &serde_json::Value, identity: &serde_json::Value)
    -> Result<(), String>
{
    let dir = accounts_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    keychain_write(&slot_service(slot), &blob.to_string())?;
    let p = dir.join(format!("{slot}.json"));
    let body = serde_json::to_string_pretty(identity).map_err(|e| e.to_string())?;
    std::fs::write(&p, body).map_err(|e| format!("{}: {e}", p.display()))
}

/// Replace `~/.claude.json` atomically, mode 0600.
///
/// Claude Code reads this file continuously. A torn write is a corrupt file for whatever
/// reads it next, so the replacement is a same-directory temp plus rename. `preserve_order`
/// is enabled on serde_json workspace-wide, which is what keeps the 59 existing keys in
/// the order Claude Code wrote them instead of silently re-sorting the whole file.
pub fn write_claude_json(v: &serde_json::Value) -> Result<(), String> {
    let path = claude_json_path();
    let dir = path.parent().ok_or("claude.json has no parent directory")?;
    let tmp = dir.join(format!(".claude.json.{}.tmp", std::process::id()));
    let body = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    let write = || -> Result<(), String> {
        std::fs::write(&tmp, &body).map_err(|e| format!("{}: {e}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("{}: {e}", tmp.display()))?;
        }
        std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))
    };
    let r = write();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// `~/.claude.json`, which is what Claude Code itself reads.
pub fn claude_json_path() -> PathBuf {
    if let Ok(p) = std::env::var("PAOS_CLAUDE_JSON") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    home().join(".claude.json")
}

/// The account uuid Claude Code is currently signed in as.
pub fn active_uuid() -> Option<String> {
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(claude_json_path()).ok()?).ok()?;
    v.get("oauthAccount")?.get("accountUuid")?.as_str().map(str::to_string)
}

/// Which slot is live, matched by account UUID rather than by email or filename.
///
/// The uuid is the only stable key: an email can be re-cased or aliased and a slot file
/// can be renamed, but the uuid is what Claude Code itself stores. Matching on anything
/// else reports the wrong account as active, which is how a switch "succeeds" into the
/// account you were already on.
pub fn current_slot() -> Option<String> {
    let uuid = active_uuid()?;
    list_slots().into_iter().find(|slot| {
        load_slot_identity(slot)
            .and_then(|id| {
                id.get("oauthAccount")?.get("accountUuid")?.as_str().map(str::to_string)
            })
            .is_some_and(|u| u == uuid)
    })
}

/// The next slot in rotation after the live one.
///
/// Deliberately dumb round-robin, and NOT the switch picker: `claude-acct next` means
/// "give me the one after this", which is what you want when stepping through accounts by
/// hand. An unknown current slot starts at the beginning rather than failing.
pub fn next_slot() -> Option<String> {
    next_of(&list_slots(), current_slot().as_deref())
}

pub fn next_of(slots: &[String], current: Option<&str>) -> Option<String> {
    if slots.is_empty() {
        return None;
    }
    match current.and_then(|c| slots.iter().position(|s| s == c)) {
        Some(i) => Some(slots[(i + 1) % slots.len()].clone()),
        None => Some(slots[0].clone()),
    }
}

/// Can this process write the keychain?
///
/// Read and write are NOT symmetric here, and assuming they are is the bug: a sandboxed
/// session reads fine and gets rc=161 on write. Callers use this to refuse with an
/// explanation instead of failing mid-switch, which leaves the credentials half-moved.
pub fn keychain_writable() -> bool {
    // Probe with a value we then remove, under a name that cannot collide with a real
    // slot. Writing nothing is not an option: the denial only surfaces on a real write.
    let svc = format!("{LIVE_SERVICE}-paos-writeprobe");
    let user = std::env::var("USER").unwrap_or_default();
    let ok = Command::new("security")
        .args(["add-generic-password", "-U", "-a", &user, "-s", &svc, "-w", "probe"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok {
        let _ = Command::new("security")
            .args(["delete-generic-password", "-s", &svc, "-a", &user])
            .output();
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_real_identity_is_baked_into_the_workspace() {
        // Walks EVERY crate, not a hand-listed few. The first version listed five files
        // and passed while four other crates still named the operator — a guard that
        // checks a subset reports a clean tree it never looked at.
        //
        // Split halves, joined at runtime: a test that greps the source for a string
        // cannot CONTAIN that string, and the first version failed on its own banned
        // list. Worse, the bulk substitution then "fixed" the LIST rather than the
        // fixtures — silently weakening the very test meant to prevent this.
        let banned: Vec<String> = [("mr6", "r1n"), ("with", "flare"), ("scani", ".xyz"),
                                   ("mgrin", "_eth"), ("scaney", "_bot"),
                                   ("mgrin", "_global_memory"), ("Flare", "XYZ")]
            .iter().map(|(a, b)| format!("{a}{b}")).collect();

        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent().expect("crates/");
        let mut checked = 0;
        let mut stack = vec![crates.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    if p.file_name().is_some_and(|n| n == "target") {
                        continue;
                    }
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let Ok(src) = std::fs::read_to_string(&p) else { continue };
                    checked += 1;
                    for b in &banned {
                        assert!(!src.contains(b.as_str()),
                                "{} contains {b:?} — use an example identity", p.display());
                    }
                }
            }
        }
        // Without this the test passes silently the day the walk stops finding anything.
        assert!(checked > 50, "only walked {checked} files; the scan has stopped working");
    }

    #[test]
    fn a_slot_name_is_derived_from_an_email_the_same_way_python_derives_it() {
        // This names a keychain item AND a file. A drift orphans an existing slot rather
        // than erroring — the account is simply no longer found.
        assert_eq!(slot_from_email("Second@Example.COM"), "second_example.com");
        assert_eq!(slot_from_email("first@example.com"), "first_example.com");
        assert_eq!(slot_from_email("  spaced @ out .com "), "spaced_out_.com");
        // Runs of unsafe characters collapse to ONE underscore, and the edges are stripped.
        assert_eq!(slot_from_email("a+++b@x.com"), "a_b_x.com");
        assert_eq!(slot_from_email("@@@"), "account");
        assert_eq!(slot_from_email(""), "account");
    }

    #[test]
    fn slot_names_match_the_python_on_every_shape_that_bit() {
        // Verified against claude_accounts.slot_from_email itself over 18 inputs, not
        // against my reading of its regex — including a literal TAB, non-ASCII, and the
        // underscore edge cases, which are where a hand-rolled character filter drifts.
        // (My first comparison harness used a tab-separated file and the tab case broke
        // the FORMAT, reporting a divergence that did not exist.)
        for (input, expect) in [
            ("Second@Example.COM",    "second_example.com"),
            ("  spaced @ out .com ","spaced_out_.com"),
            ("a+++b@x.com",         "a_b_x.com"),
            ("UPPER@CASE.COM",      "upper_case.com"),
            ("dots...@x.com",       "dots..._x.com"),
            ("tab\the@z.co",        "tab_he_z.co"),
            ("_leading@x.com",      "leading_x.com"),
            // Only the EDGES are stripped: an underscore run in the middle survives.
            ("trailing_@x.com",     "trailing__x.com"),
            ("\u{4e2d}\u{6587}@x.com", "x.com"),
            ("a--b@x.com",          "a--b_x.com"),
            ("a@b@c.com",           "a_b_c.com"),
            // "..._..." and NOT "account": `strip("_")` removes only UNDERSCORES from the
            // edges, so a name whose edges are dots survives. I hand-wrote "account" here
            // from intuition after having already MEASURED the real answer, and the test
            // caught me — which is the whole argument for copying measured values rather
            // than re-deriving them.
            ("...@...",             "..._..."),
            ("-@-",                 "-_-"),
            ("@@@",                 "account"),
            ("",                    "account"),
        ] {
            assert_eq!(slot_from_email(input), expect, "input {input:?}");
        }
    }

    #[test]
    fn the_slot_service_name_is_scoped_under_the_live_one() {
        assert_eq!(slot_service("a_b.com"), "Claude Code-credentials-a_b.com");
    }

    #[test]
    fn no_accounts_directory_means_no_slots_rather_than_an_error() {
        // A machine that has never captured an account is a normal first-run state, not a
        // failure. Erroring here would make `accounts list` look broken on a fresh Mac.
        let d = std::env::temp_dir().join(format!("paos-slots-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        assert!(list_slots_at(&d).is_empty());
    }

    #[test]
    fn slots_are_listed_by_filename_sorted_and_only_json() {
        let d = std::env::temp_dir().join(format!("paos-slots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for f in ["b.json", "a.json", "notes.txt", "c.json"] {
            std::fs::write(d.join(f), "{}").unwrap();
        }
        assert_eq!(list_slots_at(&d), vec!["a", "b", "c"],
                   "sorted, .json only, and the extension stripped");
    }
}
