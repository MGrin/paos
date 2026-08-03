//! Switching the live Claude account. The WRITE half of the accounts cluster.
//!
//! Ported from `use_slot` in `claude_accounts.py`. Two credentials have to move together
//! — the keychain blob and the identity in `~/.claude.json` — and every rule here is
//! about what happens when the second one fails after the first one succeeded.
//!
//! **This refuses to run where it cannot finish.** A sandboxed agent session reads the
//! keychain fine and gets rc=161 (`errSecInteractionNotAllowed`) on write, so a switch
//! attempted from a session would get partway and stop. The Python died with a
//! `RuntimeError` traceback, which is how a failed switch looked like a crash. The
//! pre-flight probe turns that into one sentence naming where to run it instead.

use serde_json::Value;

/// Everything a switch touches. Behind a trait so the ORDERING and ROLLBACK rules are
/// testable — every one of them is a rule about a failure, and a live test cannot arrange
/// a keychain write to fail on demand.
pub trait Vault {
    /// Can this process write the keychain at all?
    fn writable(&self) -> bool;
    fn slot_blob(&self, slot: &str) -> Result<Value, String>;
    fn slot_identity(&self, slot: &str) -> Result<Value, String>;
    fn live_blob(&self) -> Result<Value, String>;
    fn write_live(&self, blob: &Value) -> Result<(), String>;
    /// Stash a blob + identity into a slot.
    fn write_slot(&self, slot: &str, blob: &Value, identity: &Value) -> Result<(), String>;
    fn read_claude_json(&self) -> Result<Value, String>;
    fn write_claude_json(&self, v: &Value) -> Result<(), String>;
    fn current_slot(&self) -> Option<String>;
}

/// The identity fields `~/.claude.json` carries for an account.
pub fn identity_from_claude_json(cj: &Value) -> Value {
    serde_json::json!({
        "userID": cj.get("userID").cloned().unwrap_or(Value::Null),
        "oauthAccount": cj.get("oauthAccount").cloned().unwrap_or(serde_json::json!({})),
    })
}

/// What went wrong, in the caller's terms.
#[derive(Debug, Clone, PartialEq)]
pub enum SwitchError {
    /// The keychain is not writable from here — almost always an agent sandbox.
    ReadOnly,
    Failed(String),
}

impl std::fmt::Display for SwitchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SwitchError::ReadOnly => f.write_str(
                "cannot switch accounts from here: the keychain is read-only in this \
                 process (errSecInteractionNotAllowed). Run it in a terminal.",
            ),
            SwitchError::Failed(e) => f.write_str(e),
        }
    }
}

/// Re-stash the LIVE credentials into the slot they belong to, before they are overwritten.
///
/// **This is not housekeeping, it is what stops a slot bricking.** The CLI rotates refresh
/// tokens while an account is live, so by switch-out time the slot's stashed copy is
/// stale; its next refresh answers 400 `invalid_grant` and the account needs a manual
/// re-login. Errors are swallowed on purpose: a broken outgoing stash must never block the
/// switch itself, because the switch is the thing the user is waiting on.
fn restash_outgoing(v: &dyn Vault, target: &str) {
    let Some(outgoing) = v.current_slot() else { return };
    if outgoing == target {
        return;
    }
    let (Ok(blob), Ok(cj)) = (v.live_blob(), v.read_claude_json()) else { return };
    let _ = v.write_slot(&outgoing, &blob, &identity_from_claude_json(&cj));
}

/// Stash whatever account is signed in right now into a slot.
///
/// With no slot given the name is derived from the account's own email, so `capture` is
/// safe to run repeatedly: logging into an account and capturing it twice overwrites its
/// own slot rather than creating a second one. Returns the slot it wrote.
pub fn capture_current(v: &dyn Vault, slot: Option<&str>) -> Result<String, SwitchError> {
    if !v.writable() {
        return Err(SwitchError::ReadOnly);
    }
    let blob = v.live_blob().map_err(SwitchError::Failed)?;
    let cj = v.read_claude_json().map_err(SwitchError::Failed)?;
    let identity = identity_from_claude_json(&cj);
    let slot = match slot {
        Some(s) => s.to_string(),
        None => {
            let email = identity
                .get("oauthAccount")
                .and_then(|o| o.get("emailAddress"))
                .and_then(|e| e.as_str())
                .unwrap_or("account");
            crate::slots::slot_from_email(email)
        }
    };
    v.write_slot(&slot, &blob, &identity).map_err(SwitchError::Failed)?;
    Ok(slot)
}

/// Make `slot` the live account.
///
/// `refresher` runs before anything is written, so a slot whose token needs refreshing is
/// refreshed while the old account is still live and recoverable.
pub fn use_slot(
    v: &dyn Vault,
    slot: &str,
    refresher: Option<&dyn Fn(&str, &Value) -> Result<Value, String>>,
) -> Result<(), SwitchError> {
    // FIRST, before reading anything: refuse where the write cannot land. Discovering
    // this halfway through leaves the credentials moved and the identity not, which is
    // the one state nothing here can recover from.
    if !v.writable() {
        return Err(SwitchError::ReadOnly);
    }

    let mut blob = v.slot_blob(slot).map_err(SwitchError::Failed)?;
    let identity = v.slot_identity(slot).map_err(SwitchError::Failed)?;
    if let Some(r) = refresher {
        blob = r(slot, &blob).map_err(SwitchError::Failed)?;
    }

    restash_outgoing(v, slot);

    // Remember the outgoing credential so the swap can be undone. A missing live
    // credential is not an error — a machine mid-setup has none — it just means there is
    // nothing to roll back to.
    let prev = v.live_blob().ok();

    v.write_live(&blob).map_err(SwitchError::Failed)?;

    // The identity update is the half that can fail after the credentials have already
    // moved. If it does, put the old credentials back: a live blob for one account and an
    // identity for another is a state Claude Code cannot make sense of, and it presents
    // as being signed in as somebody you are not.
    let result = (|| {
        let mut cj = v.read_claude_json()?;
        cj["oauthAccount"] = identity.get("oauthAccount").cloned().unwrap_or(serde_json::json!({}));
        match identity.get("userID") {
            Some(u) if !u.is_null() => cj["userID"] = u.clone(),
            _ => {}
        }
        v.write_claude_json(&cj)
    })();

    if let Err(e) = result {
        if let Some(prev) = prev {
            let _ = v.write_live(&prev);
        }
        return Err(SwitchError::Failed(e));
    }
    Ok(())
}

/// The real vault: keychain plus `~/.claude.json`.
pub struct Live;

impl Vault for Live {
    fn writable(&self) -> bool {
        crate::slots::keychain_writable()
    }
    fn slot_blob(&self, slot: &str) -> Result<Value, String> {
        crate::slots::load_slot_blob(slot)
    }
    fn slot_identity(&self, slot: &str) -> Result<Value, String> {
        crate::slots::load_slot_identity(slot)
            .ok_or_else(|| format!("no identity stashed for '{slot}'"))
    }
    fn live_blob(&self) -> Result<Value, String> {
        crate::slots::read_live_blob()
    }
    fn write_live(&self, blob: &Value) -> Result<(), String> {
        crate::slots::keychain_write(crate::slots::LIVE_SERVICE, &blob.to_string())
    }
    fn write_slot(&self, slot: &str, blob: &Value, identity: &Value) -> Result<(), String> {
        crate::slots::save_slot(slot, blob, identity)
    }
    fn read_claude_json(&self) -> Result<Value, String> {
        let p = crate::slots::claude_json_path();
        let s = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        serde_json::from_str(&s).map_err(|e| format!("{}: {e}", p.display()))
    }
    fn write_claude_json(&self, v: &Value) -> Result<(), String> {
        crate::slots::write_claude_json(v)
    }
    fn current_slot(&self) -> Option<String> {
        crate::slots::current_slot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Fake {
        writable: bool,
        current: Option<String>,
        live: Option<Value>,
        cj: Value,
        blobs: std::collections::BTreeMap<String, Value>,
        idents: std::collections::BTreeMap<String, Value>,
        cj_write_fails: bool,
        slot_write_fails: bool,
        calls: RefCell<Vec<String>>,
        live_written: RefCell<Vec<Value>>,
        cj_written: RefCell<Vec<Value>>,
        slots_written: RefCell<Vec<(String, Value)>>,
    }

    fn blob(tok: &str) -> Value {
        serde_json::json!({ "claudeAiOauth": { "accessToken": tok } })
    }

    fn ident(uuid: &str, user: &str) -> Value {
        serde_json::json!({
            "userID": user,
            "oauthAccount": { "accountUuid": uuid, "emailAddress": "a@b.c" },
        })
    }

    impl Vault for Fake {
        fn writable(&self) -> bool {
            self.calls.borrow_mut().push("writable".into());
            self.writable
        }
        fn slot_blob(&self, slot: &str) -> Result<Value, String> {
            self.calls.borrow_mut().push(format!("slot_blob:{slot}"));
            self.blobs.get(slot).cloned().ok_or_else(|| format!("NoCredentials: {slot}"))
        }
        fn slot_identity(&self, slot: &str) -> Result<Value, String> {
            self.idents.get(slot).cloned().ok_or_else(|| format!("no identity: {slot}"))
        }
        fn live_blob(&self) -> Result<Value, String> {
            self.live.clone().ok_or_else(|| "NoCredentials: live".to_string())
        }
        fn write_live(&self, b: &Value) -> Result<(), String> {
            self.calls.borrow_mut().push("write_live".into());
            self.live_written.borrow_mut().push(b.clone());
            Ok(())
        }
        fn write_slot(&self, slot: &str, b: &Value, _i: &Value) -> Result<(), String> {
            self.calls.borrow_mut().push(format!("write_slot:{slot}"));
            if self.slot_write_fails {
                return Err("keychain write failed".into());
            }
            self.slots_written.borrow_mut().push((slot.into(), b.clone()));
            Ok(())
        }
        fn read_claude_json(&self) -> Result<Value, String> {
            Ok(self.cj.clone())
        }
        fn write_claude_json(&self, v: &Value) -> Result<(), String> {
            self.calls.borrow_mut().push("write_claude_json".into());
            if self.cj_write_fails {
                return Err("claude.json: Permission denied".into());
            }
            self.cj_written.borrow_mut().push(v.clone());
            Ok(())
        }
        fn current_slot(&self) -> Option<String> {
            self.current.clone()
        }
    }

    fn ready() -> Fake {
        let mut f = Fake {
            writable: true,
            current: Some("old".into()),
            live: Some(blob("LIVE_OLD")),
            cj: serde_json::json!({
                "numStartups": 7,
                "oauthAccount": { "accountUuid": "uuid-old" },
                "userID": "user-old",
                "somethingElse": { "keep": true },
            }),
            ..Default::default()
        };
        f.blobs.insert("new".into(), blob("STASHED_NEW"));
        f.idents.insert("new".into(), ident("uuid-new", "user-new"));
        f
    }

    #[test]
    fn a_read_only_keychain_refuses_before_touching_anything() {
        // The sandbox case. Discovering rc=161 halfway leaves the credentials moved and
        // the identity not — the one state this code cannot recover from.
        let mut f = ready();
        f.writable = false;
        let e = use_slot(&f, "new", None).unwrap_err();
        assert_eq!(e, SwitchError::ReadOnly);
        assert!(e.to_string().contains("terminal"), "it must say where to run it: {e}");
        assert_eq!(f.calls.borrow().as_slice(), &["writable"],
                   "nothing was read or written: {:?}", f.calls.borrow());
    }

    #[test]
    fn the_outgoing_accounts_live_credentials_are_restashed_before_being_overwritten() {
        // The CLI rotates refresh tokens while an account is live, so the slot's stashed
        // copy is stale by switch-out time. Without this the outgoing slot's next refresh
        // answers 400 invalid_grant and needs a manual re-login.
        let f = ready();
        use_slot(&f, "new", None).unwrap();

        let stashed = f.slots_written.borrow().clone();
        assert_eq!(stashed.len(), 1);
        assert_eq!(stashed[0].0, "old");
        assert_eq!(stashed[0].1, blob("LIVE_OLD"), "the LIVE blob, not the stale stash");

        let calls = f.calls.borrow().clone();
        let i_stash = calls.iter().position(|c| c == "write_slot:old").unwrap();
        let i_live = calls.iter().position(|c| c == "write_live").unwrap();
        assert!(i_stash < i_live, "restash must precede the overwrite: {calls:?}");
    }

    #[test]
    fn a_failed_restash_does_not_block_the_switch() {
        // The switch is what the user is waiting on. A broken outgoing stash costs that
        // account a re-login later; refusing the switch costs the work happening now.
        let mut f = ready();
        f.slot_write_fails = true;
        use_slot(&f, "new", None).unwrap();
        assert_eq!(f.live_written.borrow()[0], blob("STASHED_NEW"));
    }

    #[test]
    fn switching_to_the_account_already_live_does_not_restash_over_itself() {
        let mut f = ready();
        f.current = Some("new".into());
        use_slot(&f, "new", None).unwrap();
        assert!(f.slots_written.borrow().is_empty(),
                "no self-stash: {:?}", f.slots_written.borrow());
    }

    #[test]
    fn a_failed_identity_write_rolls_the_credentials_back() {
        // Live credentials for one account plus an identity for another presents as being
        // signed in as somebody you are not, and nothing detects it.
        let mut f = ready();
        f.cj_write_fails = true;
        let e = use_slot(&f, "new", None).unwrap_err();
        assert!(matches!(e, SwitchError::Failed(_)));

        let writes = f.live_written.borrow().clone();
        assert_eq!(writes.len(), 2, "written, then put back: {writes:?}");
        assert_eq!(writes[0], blob("STASHED_NEW"));
        assert_eq!(writes[1], blob("LIVE_OLD"), "the OLD credentials are restored");
    }

    #[test]
    fn with_no_previous_credentials_there_is_nothing_to_roll_back_to() {
        // A machine mid-setup has no live item. That is not an error and must not become
        // one — but it also must not write `null` over the keychain on rollback.
        let mut f = ready();
        f.live = None;
        f.cj_write_fails = true;
        let _ = use_slot(&f, "new", None).unwrap_err();
        assert_eq!(f.live_written.borrow().len(), 1, "no rollback write");
    }

    #[test]
    fn the_identity_update_replaces_only_the_two_account_keys() {
        // ~/.claude.json is 58KB and 59 top-level keys of Claude Code's own state.
        // Rewriting anything but oauthAccount and userID would discard it.
        let f = ready();
        use_slot(&f, "new", None).unwrap();
        let w = &f.cj_written.borrow()[0];
        assert_eq!(w["oauthAccount"]["accountUuid"], serde_json::json!("uuid-new"));
        assert_eq!(w["userID"], serde_json::json!("user-new"));
        assert_eq!(w["numStartups"], serde_json::json!(7), "untouched");
        assert_eq!(w["somethingElse"], serde_json::json!({ "keep": true }), "untouched");
        assert_eq!(w.as_object().unwrap().len(), 4, "no key added or dropped");
    }

    #[test]
    fn a_slot_with_no_stashed_user_id_leaves_the_existing_one_alone() {
        // The Python writes userID only when it is not None. Writing null would sign the
        // CLI in with no user id at all.
        let mut f = ready();
        f.idents.insert("new".into(), serde_json::json!({
            "userID": Value::Null,
            "oauthAccount": { "accountUuid": "uuid-new" },
        }));
        use_slot(&f, "new", None).unwrap();
        assert_eq!(f.cj_written.borrow()[0]["userID"], serde_json::json!("user-old"));
    }

    #[test]
    fn the_refresher_runs_before_anything_is_written() {
        // A slot whose token needs refreshing must be refreshed while the old account is
        // still live: if the refresh fails, nothing has moved and the machine is intact.
        let f = ready();
        let r = |_slot: &str, _b: &Value| -> Result<Value, String> { Ok(blob("REFRESHED")) };
        use_slot(&f, "new", Some(&r)).unwrap();
        assert_eq!(f.live_written.borrow()[0], blob("REFRESHED"));

        let f2 = ready();
        let bad = |_s: &str, _b: &Value| -> Result<Value, String> { Err("token dead".into()) };
        let e = use_slot(&f2, "new", Some(&bad)).unwrap_err();
        assert_eq!(e, SwitchError::Failed("token dead".into()));
        assert!(f2.live_written.borrow().is_empty(), "nothing moved");
        assert!(f2.slots_written.borrow().is_empty(), "not even the restash");
    }

    #[test]
    fn a_missing_slot_fails_before_any_write() {
        let f = ready();
        let e = use_slot(&f, "nope", None).unwrap_err();
        assert!(matches!(e, SwitchError::Failed(ref m) if m.contains("NoCredentials")));
        assert!(f.live_written.borrow().is_empty());
    }
}
