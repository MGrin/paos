//! `paos config` — the small settings store the daemon, librarian and dashboard share.
//!
//! Ported from `config_facet.py`. The port is not cosmetic: the Python wrote
//! `paos_config` DIRECTLY, from whichever sessions happened to run `paos config set`,
//! while paosd reads the same table every tick. That is exactly the multi-writer
//! arrangement the daemon exists to remove — unnoticed only because the table is tiny and
//! the writes are rare. Writes now go through the daemon like every other write.

/// The keys the dashboard Settings page manages, with type and default.
///
/// Static, so `config schema` needs neither the daemon nor the database — the dashboard
/// asks for this before anything has ever been written. Kept as one JSON literal so the
/// UI and CLI cannot drift on the surface they both render.
pub const SCHEMA_JSON: &str = r#"[
  {"key": "dream_enabled", "type": "bool", "default": "1", "label": "Nightly dream",
   "help": "Distill past sessions into candidate memories overnight."},
  {"key": "dream_backend", "type": "enum", "options": ["claude", "local"],
   "default": "claude", "label": "Distill backend",
   "help": "claude = Claude Code CLI on your subscription (no local RAM); local = LM Studio."},
  {"key": "dream_hour_start", "type": "int", "default": "3", "label": "Overnight start (hour)",
   "help": "Local hour the nightly window opens (0-23)."},
  {"key": "dream_hour_end", "type": "int", "default": "6", "label": "Overnight end (hour)",
   "help": "Local hour the nightly window closes (exclusive)."},
  {"key": "dream_nightly_limit", "type": "int", "default": "8", "label": "Sessions per night",
   "help": "How many recent sessions the nightly pass distills."},
  {"key": "dream_nightly_since", "type": "str", "default": "26h", "label": "Look back",
   "help": "Only sessions newer than this (e.g. 26h, 3d)."},
  {"key": "account_alerts", "type": "bool", "default": "0",
   "label": "Alert when a Claude account hits its weekly cap",
   "help": "Off by default - use /panel or /accounts to check on demand. On, you get one Telegram notice per crossing instead of finding out when work stops."},
  {"key": "digest_enabled", "type": "bool", "default": "0",
   "label": "Daily review-queue digest",
   "help": "Off by default. `/digest` gives the same summary on demand."},
  {"key": "identity_global_dataset", "type": "str", "default": "global_memory",
   "label": "Global memory dataset",
   "help": "Where facts that are true everywhere are filed. Was a compile-time constant."},
  {"key": "identity_work_owners", "type": "str", "default": "",
   "label": "Work git owners",
   "help": "Comma-separated git owners treated as work in standup. Empty means no split."},
  {"key": "telegram_bot_token", "type": "secret", "default": "",
   "label": "Telegram bot token",
   "help": "A reference, never the token: keychain:paos/telegram_bot_token or env:NAME."},
  {"key": "telegram_chat_id", "type": "str", "default": "",
   "label": "Telegram group chat id",
   "help": "Discovered by `paos init` from your first message to the bot."},
  {"key": "telegram_allowed_user_id", "type": "str", "default": "",
   "label": "Allowed Telegram user id",
   "help": "The only user allowed to drive the bus through the bridge."},
  {"key": "telegram_operator_username", "type": "str", "default": "",
   "label": "Your Telegram @username",
   "help": "What @operator renders as, so a mention actually pings you."}
]"#;

/// Read every setting straight from the database, for when the socket is blocked.
///
/// Reads are safe under WAL, and a config read that failed inside a sandbox would break
/// every consumer that asks for a knob before doing its work.
pub fn read_only_all(db: &std::path::Path) -> Option<Vec<(String, String)>> {
    let conn = rusqlite::Connection::open_with_flags(
        db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let mut stmt = conn.prepare("SELECT key, value FROM paos_config ORDER BY key").ok()?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .ok()?
        .flatten()
        .collect();
    Some(rows)
}

/// Turn `key=value` reply lines into what the caller actually asked for.
///
/// `get` prints the bare value and nothing else, because callers substitute it straight
/// into a command. An unset key prints an empty line rather than an error: consumers all
/// fall back to their own default, and the Python behaved this way.
pub fn shape(lines: Vec<String>, get_key: Option<&str>) -> Vec<String> {
    match get_key {
        None => lines,
        Some(k) => vec![lines
            .iter()
            .find_map(|l| l.split_once('=').filter(|(key, _)| *key == k).map(|(_, v)| v.to_string()))
            .unwrap_or_default()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_is_valid_json_the_dashboard_can_render() {
        // The dashboard fetches this verbatim; a stray comma would blank the Settings page
        // with no error anywhere near the cause.
        let v: serde_json::Value = serde_json::from_str(SCHEMA_JSON).expect("valid JSON");
        assert!(v.as_array().unwrap().len() >= 8);
    }

    #[test]
    fn every_setting_declares_a_key_type_and_default() {
        let v: serde_json::Value = serde_json::from_str(SCHEMA_JSON).unwrap();
        for s in v.as_array().unwrap() {
            for field in ["key", "type", "default", "label", "help"] {
                assert!(s.get(field).is_some(), "{} missing {field}", s["key"]);
            }
        }
    }

    #[test]
    fn identity_and_telegram_are_configurable() {
        // These were compile-time constants and env vars, which is the same as saying
        // this machine is the only one that can run paos.
        let v: serde_json::Value = serde_json::from_str(SCHEMA_JSON).unwrap();
        let keys: Vec<&str> = v.as_array().unwrap()
            .iter().map(|s| s["key"].as_str().unwrap()).collect();
        for k in ["identity_global_dataset", "identity_work_owners", "telegram_bot_token",
                  "telegram_chat_id", "telegram_allowed_user_id",
                  "telegram_operator_username"] {
            assert!(keys.contains(&k), "{k} is not configurable");
        }
    }

    #[test]
    fn no_default_names_a_person_or_a_company() {
        // A default that ships someone's identity is how the next install silently files
        // its memories under a stranger's name.
        let v: serde_json::Value = serde_json::from_str(SCHEMA_JSON).unwrap();
        for s in v.as_array().unwrap() {
            let d = s["default"].as_str().unwrap().to_lowercase();
            for banned in ["mgrin", "examplecorp", "second_user", "scaney"] {
                assert!(!d.contains(banned), "{} defaults to {d}", s["key"]);
            }
        }
    }

    #[test]
    fn a_secret_defaults_to_empty_so_nothing_is_ever_a_value() {
        // A non-empty default on a secret key would be a credential in the source tree.
        let v: serde_json::Value = serde_json::from_str(SCHEMA_JSON).unwrap();
        let mut seen = 0;
        for s in v.as_array().unwrap() {
            if s["type"] == "secret" {
                seen += 1;
                assert_eq!(s["default"], "", "{} ships a value", s["key"]);
            }
        }
        // Without this the test passes vacuously the day the secret type is renamed.
        assert!(seen > 0, "no key declares type secret");
    }

    #[test]
    fn get_prints_the_bare_value_only() {
        // Callers substitute this straight into a command; "dream_hour_start=4" would be
        // pasted in whole.
        let out = shape(vec!["dream_hour_start=4".into(), "dream_enabled=1".into()],
                        Some("dream_hour_start"));
        assert_eq!(out, vec!["4"]);
    }

    #[test]
    fn get_on_an_unset_key_is_empty_not_an_error() {
        // Every consumer falls back to its own default; erroring would make a fresh
        // machine fail before anything had ever been written.
        assert_eq!(shape(vec!["a=1".into()], Some("nope")), vec![""]);
    }

    #[test]
    fn get_does_not_match_a_key_by_prefix() {
        assert_eq!(shape(vec!["dream_hour_start=3".into()], Some("dream_hour")), vec![""]);
    }

    #[test]
    fn list_passes_every_pair_through() {
        let out = shape(vec!["a=1".into(), "b=2".into()], None);
        assert_eq!(out, vec!["a=1", "b=2"]);
    }

    #[test]
    fn a_value_containing_equals_survives_a_get() {
        // e.g. a query string or a flag list; splitting on every '=' would truncate it.
        assert_eq!(shape(vec!["k=a=b".into()], Some("k")), vec!["a=b"]);
    }
}
