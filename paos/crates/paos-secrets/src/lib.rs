//! Secrets, held by reference.
//!
//! `paos_config` stores a POINTER to a secret, never the secret. That is what lets the
//! settings page say "configured" or "missing" without the web layer ever being able to
//! read a token — and it keeps `paos.db`, which also holds every memory and every bus
//! message, out of the business of being a credential store.

/// Where a secret actually lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reference {
    /// macOS Keychain, read with `security find-generic-password`.
    Keychain { service: String, account: String },
    /// An environment variable, or a `KEY=value` line in the `.env` beside the store.
    Env { name: String },
    /// Nothing configured — a state, not an error.
    Unset,
}

/// Parse a stored reference.
///
/// Anything unrecognised is `Unset`, INCLUDING a bare value. If someone pastes a token
/// into the row, silently using it would turn the database into a secret store by
/// accident — the one outcome this design exists to prevent.
pub fn parse(raw: &str) -> Reference {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix("keychain:") {
        if let Some((service, account)) = rest.split_once('/') {
            if !service.is_empty() && !account.is_empty() {
                return Reference::Keychain {
                    service: service.to_string(),
                    account: account.to_string(),
                };
            }
        }
        return Reference::Unset;
    }
    if let Some(name) = raw.strip_prefix("env:") {
        if !name.is_empty() {
            return Reference::Env { name: name.to_string() };
        }
    }
    Reference::Unset
}

/// What the settings page is allowed to know about a secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Configured,
    /// Nothing is set. The normal first-run state.
    Missing,
    /// A reference exists but its backend cannot be read here — for example a
    /// `keychain:` reference on Linux. Deliberately distinct from `Missing`: an empty
    /// token and an unreachable backend fail identically at the API, and only one of them
    /// is the user's fault.
    Unreadable,
}

/// Where `paos init` should PUT a new secret on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Keychain,
    Env,
}

/// The platform's storage. Writing is platform-dependent; READING never is — a
/// `keychain:` reference on Linux is Unreadable, reported, not silently empty.
pub fn default_backend() -> Backend {
    if cfg!(target_os = "macos") { Backend::Keychain } else { Backend::Env }
}

pub fn resolve(raw: &str, env_lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    match parse(raw) {
        Reference::Env { name } => env_lookup(&name).filter(|v| !v.is_empty()),
        Reference::Keychain { service, account } => keychain_read(&service, &account),
        Reference::Unset => None,
    }
}

pub fn status(raw: &str, env_lookup: &dyn Fn(&str) -> Option<String>) -> Status {
    status_with_backend(raw, env_lookup, default_backend())
}

/// The backend is a parameter so the platform branch is testable on one machine.
pub fn status_with_backend(
    raw: &str,
    env_lookup: &dyn Fn(&str) -> Option<String>,
    backend: Backend,
) -> Status {
    match parse(raw) {
        Reference::Unset => Status::Missing,
        Reference::Env { name } => match env_lookup(&name).filter(|v| !v.is_empty()) {
            Some(_) => Status::Configured,
            None => Status::Missing,
        },
        Reference::Keychain { service, account } => {
            if backend != Backend::Keychain {
                return Status::Unreadable;
            }
            match keychain_read(&service, &account) {
                Some(_) => Status::Configured,
                None => Status::Missing,
            }
        }
    }
}

/// The environment variable an account maps to. Derived, never invented, so `resolve`
/// and `store` cannot disagree about where a secret went.
pub fn env_name(account: &str) -> String {
    format!("PAOS_{}", account.to_uppercase().replace(['-', '.', ' '], "_"))
}

/// Write a secret and return the REFERENCE to record in `paos_config`.
///
/// Never returns or logs the value. On macOS this goes to the Keychain through
/// `security(1)` reading the value on STDIN — `-w <value>` would put the token in argv,
/// where `ps` publishes it to every process on the machine for the life of the call.
pub fn store(
    backend: Backend,
    service: &str,
    account: &str,
    value: &str,
    env_path: &std::path::Path,
) -> Result<String, String> {
    match backend {
        Backend::Keychain => {
            keychain_write(service, account, value)?;
            Ok(format!("keychain:{service}/{account}"))
        }
        Backend::Env => {
            let name = env_name(account);
            write_env_line(env_path, &name, value)?;
            Ok(format!("env:{name}"))
        }
    }
}

fn keychain_write(service: &str, account: &str, value: &str) -> Result<(), String> {
    use std::io::Write;
    let mut child = std::process::Command::new("security")
        .args(["add-generic-password", "-s", service, "-a", account, "-U", "-w"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("security: {e}"))?;
    child
        .stdin
        .as_mut()
        .ok_or("no stdin")?
        .write_all(value.as_bytes())
        .map_err(|e| format!("writing the secret: {e}"))?;
    let out = child.wait_with_output().map_err(|e| format!("security: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(())
}

/// Replace the line if the key is already there, append if not — and 0600 either way.
fn write_env_line(path: &std::path::Path, name: &str, value: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|l| !l.trim_start().starts_with(&format!("{name}=")))
        .map(str::to_string)
        .collect();
    lines.push(format!("{name}={value}"));
    let body = format!("{}\n", lines.join("\n"));
    std::fs::write(path, body).map_err(|e| format!("writing {}: {e}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    Ok(())
}

/// Read one generic password. Shells `security(1)` rather than binding the Security
/// framework: macOS ships the binary, and a binding would cost every platform a build
/// dependency to serve one.
fn keychain_read(service: &str, account: &str) -> Option<String> {
    let out = std::process::Command::new("security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if v.is_empty() { None } else { Some(v) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_keychain_reference_splits_into_service_and_account() {
        assert_eq!(
            parse("keychain:paos/telegram_bot_token"),
            Reference::Keychain { service: "paos".into(), account: "telegram_bot_token".into() }
        );
    }

    #[test]
    fn an_env_reference_names_a_variable() {
        assert_eq!(parse("env:TELEGRAM_BOT_TOKEN"),
                   Reference::Env { name: "TELEGRAM_BOT_TOKEN".into() });
    }

    #[test]
    fn empty_and_whitespace_are_unset_not_an_error() {
        // A fresh install has never written this row. Unset is a state, not a failure,
        // and treating it as one would make the settings page red on first run.
        assert_eq!(parse(""), Reference::Unset);
        assert_eq!(parse("   "), Reference::Unset);
    }

    #[test]
    fn a_bare_value_is_refused_rather_than_treated_as_a_secret() {
        // The whole design is that a VALUE never lands in the database. If someone pastes
        // a token into the row, we must not quietly start using it — that would make the
        // database a secret store by accident, which is exactly what this prevents.
        assert_eq!(parse("1234567:AAHsomething"), Reference::Unset);
    }

    #[test]
    fn an_unknown_scheme_is_unset() {
        assert_eq!(parse("vault:secret/paos"), Reference::Unset);
    }

    fn env_with<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn an_env_reference_resolves_to_its_value() {
        let env = env_with(&[("TELEGRAM_BOT_TOKEN", "123:abc")]);
        assert_eq!(resolve("env:TELEGRAM_BOT_TOKEN", &env), Some("123:abc".to_string()));
        assert_eq!(status("env:TELEGRAM_BOT_TOKEN", &env), Status::Configured);
    }

    #[test]
    fn an_env_reference_to_an_unset_variable_is_missing() {
        let env = env_with(&[]);
        assert_eq!(resolve("env:NOPE", &env), None);
        assert_eq!(status("env:NOPE", &env), Status::Missing);
    }

    #[test]
    fn an_unset_reference_is_missing_not_unreadable() {
        // Nothing configured yet is the normal first-run state. Reporting it as broken
        // would send someone hunting for a fault that is just an empty row.
        assert_eq!(status("", &env_with(&[])), Status::Missing);
    }

    #[test]
    fn a_keychain_reference_is_unreadable_where_there_is_no_keychain() {
        // NOT Missing. An empty token and an unreachable backend fail identically at the
        // Telegram API, and only one of them is the user's fault — so the two states must
        // never collapse into one.
        assert_eq!(status_with_backend("keychain:paos/tok", &env_with(&[]), Backend::Env),
                   Status::Unreadable);
    }

    #[test]
    fn storing_to_env_writes_a_file_only_the_owner_can_read() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("paos-sec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(".env");
        let r = store(Backend::Env, "paos", "telegram_bot_token", "123:abc", &f).unwrap();
        assert_eq!(r, "env:PAOS_TELEGRAM_BOT_TOKEN");
        let text = std::fs::read_to_string(&f).unwrap();
        assert!(text.contains("PAOS_TELEGRAM_BOT_TOKEN=123:abc"), "{text}");
        let mode = std::fs::metadata(&f).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a secrets file the group can read is not a secrets file");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn storing_twice_replaces_rather_than_appending() {
        // Two lines for the same key is a file whose meaning depends on which one the
        // reader stops at — and the readers here disagree.
        let dir = std::env::temp_dir().join(format!("paos-sec2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(".env");
        store(Backend::Env, "paos", "tok", "one", &f).unwrap();
        store(Backend::Env, "paos", "tok", "two", &f).unwrap();
        let text = std::fs::read_to_string(&f).unwrap();
        assert_eq!(text.matches("PAOS_TOK=").count(), 1, "{text}");
        assert!(text.contains("PAOS_TOK=two"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_stored_secret_is_readable_back_through_its_own_reference() {
        // store and resolve must agree on the name. They derive it from the same
        // function precisely so they cannot drift.
        let dir = std::env::temp_dir().join(format!("paos-sec3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(".env");
        let reference = store(Backend::Env, "paos", "tok", "value", &f).unwrap();
        let env = env_with(&[("PAOS_TOK", "value")]);
        assert_eq!(resolve(&reference, &env), Some("value".to_string()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_env_name_is_derived_from_the_account_not_invented() {
        assert_eq!(env_name("telegram_bot_token"), "PAOS_TELEGRAM_BOT_TOKEN");
    }

    #[test]
    fn the_default_backend_follows_the_platform() {
        let expected = if cfg!(target_os = "macos") { Backend::Keychain } else { Backend::Env };
        assert_eq!(default_backend(), expected);
    }
}
