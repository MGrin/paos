//! The two completion backends the librarian can use, and the switch between them.
//!
//! **`claude`** — the Claude Code CLI on the operator's own subscription. No API key, no
//! local RAM, and it handles a 400k-character dream chunk.
//! **`local`** — an OpenAI-compatible endpoint (LM Studio) at `http://127.0.0.1:1234/v1`.
//! Free and offline, but it loads a model.
//!
//! HTTP is done by shelling `curl`, exactly as `paos-operator::telegram` does for the Bot
//! API. That is a deliberate reuse of an existing precedent rather than a new dependency:
//! there is no HTTP client in this workspace and adding one to reach localhost would be a
//! poor trade.
//!
//! ## Everything here has a deadline
//!
//! `std::process::Command::output()` blocks until the child exits — there is NO timeout,
//! unlike Python's `subprocess.run(..., timeout=...)`. A port that reaches for `output()`
//! silently drops the guard, and the failure it drops is the one that matters: a hung
//! `claude -p` inside the nightly dream would block the daemon's dream thread forever
//! with nothing logged. `run_with_deadline` exists for that reason and every subprocess
//! here goes through it.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `MEMORY_CLAUDE_MODEL`, default `claude-haiku-4-5`.
pub const DEFAULT_CLAUDE_MODEL: &str = "claude-haiku-4-5";
/// `MEMORY_CLAUDE_TIMEOUT`, seconds.
pub const DEFAULT_CLAUDE_TIMEOUT: u64 = 180;
/// `MEMORY_LLM_TIMEOUT`, seconds.
pub const DEFAULT_LLM_TIMEOUT: u64 = 120;
/// `MEMORY_LLM_URL`.
pub const DEFAULT_LLM_URL: &str = "http://127.0.0.1:1234/v1";
/// The pinned local model. Requesting a 24B coder made this 24 GB machine unusable.
pub const DEFAULT_LLM_MODEL: &str = "google/gemma-4-e4b";

/// Substrings that mark a model small enough to load without wedging the machine.
pub const SMALL_MODEL_HINTS: &[&str] =
    &["gemma", "e4b", "nano", "mini", "phi", "-1b", "-2b", "-3b", "-4b"];

fn env_or(key: &str, default: &str) -> String {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => default.to_string(),
    }
}

fn env_secs(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

pub fn claude_model() -> String {
    env_or("MEMORY_CLAUDE_MODEL", DEFAULT_CLAUDE_MODEL)
}

pub fn claude_timeout() -> u64 {
    env_secs("MEMORY_CLAUDE_TIMEOUT", DEFAULT_CLAUDE_TIMEOUT)
}

pub fn llm_url() -> String {
    env_or("MEMORY_LLM_URL", DEFAULT_LLM_URL)
}

pub fn llm_timeout() -> u64 {
    env_secs("MEMORY_LLM_TIMEOUT", DEFAULT_LLM_TIMEOUT)
}

/// Which backend to use.
///
/// `dream_backend` from `paos_config` (the dashboard Settings page writes it) wins if it
/// names a backend we have; otherwise `MEMORY_LLM_BACKEND`; otherwise `claude`.
///
/// Takes the config value rather than reading it, so the decision is testable and the
/// caller owns the database handle.
pub fn resolve_backend(configured: Option<&str>) -> String {
    if let Some(c) = configured {
        if c == "claude" || c == "local" {
            return c.to_string();
        }
    }
    let env = env_or("MEMORY_LLM_BACKEND", "claude");
    if env == "claude" || env == "local" {
        env
    } else {
        "claude".to_string()
    }
}

/// Read `dream_backend` out of a store connection. Never fails loudly — an unreadable
/// config is a reason to use the default, not to abandon the pass.
pub fn configured_backend(conn: &rusqlite::Connection) -> Option<String> {
    conn.query_row("SELECT value FROM paos_config WHERE key='dream_backend'", [], |r| {
        r.get::<_, String>(0)
    })
    .ok()
    .filter(|v| !v.trim().is_empty())
}

/// Claude Code, resolved WITHOUT relying on an enriched PATH: launchd services run with a
/// minimal one that lacks `~/.local/bin`, where Claude Code installs.
pub fn claude_bin() -> String {
    if let Ok(v) = std::env::var("PAOS_CLAUDE_BIN") {
        if !v.is_empty() {
            return v;
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let local = format!("{home}/.local/bin/claude");
    if std::path::Path::new(&local).exists() {
        local
    } else {
        "claude".into()
    }
}

/// The single prompt handed to `claude -p`.
///
/// System and user joined by a BLANK LINE. This is the whole behaviour worth diffing —
/// everything after it belongs to the model — so it is a pure function with no side
/// effects, and the parity harness prints it.
pub fn assemble_claude_prompt(system: &str, user: &str) -> String {
    format!("{system}\n\n{user}")
}

/// The exact argv for a claude completion.
pub fn claude_argv(prompt: &str, model: &str) -> Vec<String> {
    vec![
        "-p".into(),
        prompt.into(),
        "--model".into(),
        model.into(),
        "--output-format".into(),
        "text".into(),
    ]
}

/// The exact JSON body posted to an OpenAI-compatible `/chat/completions`.
///
/// `temperature: 0` because this is extraction, not writing: the same notes must distill
/// to the same facts on a re-run, or the review queue fills with near-duplicates of
/// decisions the human already made.
pub fn lm_studio_body(model: &str, system: &str, user: &str) -> String {
    serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
        "temperature": 0,
    })
    .to_string()
}

/// Run a command with a real deadline, returning `(exit_ok, stdout, stderr)`.
///
/// `Command::output()` has no timeout. Polling `try_wait` while the child writes to a
/// pipe would deadlock once the pipe buffer fills, so stdout and stderr are drained by
/// their own threads and only the WAIT is polled.
fn run_with_deadline(
    mut cmd: Command,
    timeout: Duration,
) -> Result<(bool, String, String), String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;

    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_handle = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(p) = out_pipe.as_mut() {
            let _ = p.read_to_string(&mut s);
        }
        s
    });
    let err_handle = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(p) = err_pipe.as_mut() {
            let _ = p.read_to_string(&mut s);
        }
        s
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("wait failed: {e}")),
        }
    };

    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();
    match status {
        Some(st) => Ok((st.success(), stdout, stderr)),
        None => Err(format!("timed out after {}s", timeout.as_secs())),
    }
}

/// Log a backend failure. Never silent — every LLM failure is visible on stderr, because
/// a pass that quietly produced nothing looks exactly like a pass that found nothing.
fn log(msg: &str) {
    eprintln!("[librarian] {msg}");
}

/// One-shot completion via the Claude Code CLI. `None` on any failure.
pub fn claude_complete(system: &str, user: &str, timeout: Option<u64>) -> Option<String> {
    let prompt = assemble_claude_prompt(system, user);
    let model = claude_model();
    let mut cmd = Command::new(claude_bin());
    cmd.args(claude_argv(&prompt, &model));
    // Run from a neutral cwd (HOME), NOT the caller's: a project-local `.claude/` —
    // hooks, MCP servers, CLAUDE.md — loads on `claude -p` and can hang for minutes.
    // From HOME only the global config loads and a small completion returns in seconds.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    cmd.current_dir(&home);
    // launchd gives a minimal environment lacking USER/LOGNAME, without which macOS
    // cannot resolve the login Keychain where Claude Code keeps its OAuth token, and it
    // reports "Not logged in".
    if std::env::var("USER").is_err() {
        if let Some(u) = std::env::var("LOGNAME").ok().or_else(whoami) {
            cmd.env("USER", &u);
            cmd.env("LOGNAME", &u);
        }
    }
    cmd.env("HOME", &home);

    let secs = timeout.unwrap_or_else(claude_timeout);
    match run_with_deadline(cmd, Duration::from_secs(secs)) {
        Err(e) => {
            log(&format!("claude CLI unavailable: {e}"));
            None
        }
        Ok((false, _, stderr)) => {
            log(&format!("claude failed: {}", stderr.trim().chars().take(200).collect::<String>()));
            None
        }
        Ok((true, stdout, _)) => {
            let body = stdout.trim().to_string();
            if body.is_empty() {
                None
            } else {
                Some(body)
            }
        }
    }
}

fn whoami() -> Option<String> {
    Command::new("id")
        .arg("-un")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
}

/// Chat completion against the local OpenAI-compatible endpoint, via `curl`.
pub fn local_chat(
    model: &str,
    system: &str,
    user: &str,
    timeout: Option<u64>,
) -> Option<String> {
    let secs = timeout.unwrap_or_else(llm_timeout);
    let url = format!("{}/chat/completions", llm_url().trim_end_matches('/'));
    let body = lm_studio_body(model, system, user);
    let mut cmd = Command::new("curl");
    cmd.args([
        "-sS",
        "--max-time",
        &secs.to_string(),
        "-X",
        "POST",
        "-H",
        "Content-Type: application/json",
        "--data-binary",
        "@-",
        &url,
    ]);
    // The body goes on stdin rather than argv: a dream chunk is up to 400k characters and
    // would blow past ARG_MAX as an argument.
    cmd.stdin(Stdio::piped());
    let (ok, stdout, stderr) = match run_with_stdin(cmd, &body, Duration::from_secs(secs + 5)) {
        Ok(v) => v,
        Err(e) => {
            log(&format!("chat unavailable (model={model}): {e}"));
            return None;
        }
    };
    if !ok {
        log(&format!("chat unavailable (model={model}): curl: {}", stderr.trim()));
        return None;
    }
    parse_chat_reply(&stdout).or_else(|| {
        log(&format!("chat returned no usable content (model={model})"));
        None
    })
}

/// Pull `choices[0].message.content` out of an OpenAI-compatible reply.
pub fn parse_chat_reply(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let s = v.get("choices")?.get(0)?.get("message")?.get("content")?.as_str()?;
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn run_with_stdin(
    mut cmd: Command,
    body: &str,
    timeout: Duration,
) -> Result<(bool, String, String), String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
    if let Some(mut si) = child.stdin.take() {
        use std::io::Write;
        let _ = si.write_all(body.as_bytes());
    }
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_handle = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(p) = out_pipe.as_mut() {
            let _ = p.read_to_string(&mut s);
        }
        s
    });
    let err_handle = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(p) = err_pipe.as_mut() {
            let _ = p.read_to_string(&mut s);
        }
        s
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("wait failed: {e}")),
        }
    };
    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();
    match status {
        Some(st) => Ok((st.success(), stdout, stderr)),
        None => Err(format!("timed out after {}s", timeout.as_secs())),
    }
}

/// Pick the local chat model.
///
/// NEVER auto-selects a big coder: `MEMORY_LLM_MODEL` → the pinned model if the endpoint
/// advertises it → any advertised id matching a small-model hint → the pinned name
/// anyway (which may 404, and the caller falls back). Requesting devstral-24B made this
/// 24 GB machine unusable.
pub fn resolve_chat_model(advertised: &[String]) -> String {
    if let Ok(v) = std::env::var("MEMORY_LLM_MODEL") {
        if !v.trim().is_empty() {
            return v;
        }
    }
    if advertised.iter().any(|m| m == DEFAULT_LLM_MODEL) {
        return DEFAULT_LLM_MODEL.to_string();
    }
    for m in advertised {
        let low = m.to_lowercase();
        if SMALL_MODEL_HINTS.iter().any(|h| low.contains(h)) {
            return m.clone();
        }
    }
    DEFAULT_LLM_MODEL.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_claude_prompt_is_system_blank_line_user() {
        // This exact join is the whole behaviour worth diffing; everything after it is
        // the model's.
        assert_eq!(assemble_claude_prompt("SYS", "USER"), "SYS\n\nUSER");
    }

    #[test]
    fn the_claude_argv_carries_the_model_and_text_output() {
        let a = claude_argv("P", "claude-haiku-4-5");
        assert_eq!(a, vec!["-p", "P", "--model", "claude-haiku-4-5", "--output-format", "text"]);
    }

    #[test]
    fn the_local_body_is_a_zero_temperature_two_message_chat() {
        let b = lm_studio_body("m", "SYS", "USER");
        let v: serde_json::Value = serde_json::from_str(&b).expect("valid JSON");
        assert_eq!(v["model"], "m");
        assert_eq!(v["temperature"], 0);
        assert_eq!(v["messages"][0]["role"], "system");
        assert_eq!(v["messages"][0]["content"], "SYS");
        assert_eq!(v["messages"][1]["role"], "user");
        assert_eq!(v["messages"][1]["content"], "USER");
    }

    #[test]
    fn the_local_body_escapes_a_prompt_containing_quotes_and_newlines() {
        let b = lm_studio_body("m", "say \"hi\"\nthen stop", "a\\b");
        let v: serde_json::Value = serde_json::from_str(&b).expect("valid JSON");
        assert_eq!(v["messages"][0]["content"], "say \"hi\"\nthen stop");
        assert_eq!(v["messages"][1]["content"], "a\\b");
    }

    #[test]
    fn the_configured_backend_wins_when_it_names_one_we_have() {
        assert_eq!(resolve_backend(Some("local")), "local");
        assert_eq!(resolve_backend(Some("claude")), "claude");
    }

    #[test]
    fn an_unknown_configured_backend_is_ignored_not_obeyed() {
        // A typo in the dashboard must not disable the pass.
        assert_eq!(resolve_backend(Some("gpt4")), "claude");
        assert_eq!(resolve_backend(Some("")), "claude");
        assert_eq!(resolve_backend(None), "claude");
    }

    #[test]
    fn a_chat_reply_is_parsed_and_junk_is_not() {
        let ok = r#"{"choices":[{"message":{"content":"hello"}}]}"#;
        assert_eq!(parse_chat_reply(ok).as_deref(), Some("hello"));
        assert_eq!(parse_chat_reply(r#"{"choices":[]}"#), None);
        assert_eq!(parse_chat_reply("not json"), None);
        assert_eq!(parse_chat_reply(r#"{"error":"no model loaded"}"#), None);
        assert_eq!(parse_chat_reply(r#"{"choices":[{"message":{"content":""}}]}"#), None);
    }

    #[test]
    fn the_chat_model_never_auto_selects_a_big_coder() {
        let advertised = vec!["devstral-24b".to_string(), "qwen-coder-32b".to_string()];
        assert_eq!(
            resolve_chat_model(&advertised),
            DEFAULT_LLM_MODEL,
            "a cold 24B load makes this machine unusable"
        );
        let with_small = vec!["devstral-24b".to_string(), "phi-4".to_string()];
        assert_eq!(resolve_chat_model(&with_small), "phi-4");
        let with_pinned = vec![DEFAULT_LLM_MODEL.to_string(), "phi-4".to_string()];
        assert_eq!(resolve_chat_model(&with_pinned), DEFAULT_LLM_MODEL, "pinned wins");
        assert_eq!(resolve_chat_model(&[]), DEFAULT_LLM_MODEL);
    }

    #[test]
    fn a_subprocess_that_never_exits_is_killed_at_the_deadline() {
        // The guard Python had as subprocess.run(timeout=...) and Command::output() does
        // not. Without it a hung `claude -p` blocks the daemon's dream thread forever.
        let mut c = Command::new("sleep");
        c.arg("30");
        let started = Instant::now();
        let r = run_with_deadline(c, Duration::from_millis(300));
        assert!(r.is_err(), "must time out, not block");
        assert!(r.unwrap_err().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(5), "and must return promptly");
    }

    #[test]
    fn a_subprocess_that_exits_returns_its_output() {
        let mut c = Command::new("sh");
        c.args(["-c", "printf hello; printf oops 1>&2; exit 0"]);
        let (ok, out, err) = run_with_deadline(c, Duration::from_secs(10)).unwrap();
        assert!(ok);
        assert_eq!(out, "hello");
        assert_eq!(err, "oops");
    }

    #[test]
    fn a_failing_subprocess_is_reported_as_failing() {
        let mut c = Command::new("sh");
        c.args(["-c", "exit 3"]);
        let (ok, _, _) = run_with_deadline(c, Duration::from_secs(10)).unwrap();
        assert!(!ok);
    }

    #[test]
    fn a_large_body_survives_the_stdin_path() {
        // A dream chunk is up to 400k characters; on argv that is past ARG_MAX.
        let big = "x".repeat(400_000);
        let mut c = Command::new("wc");
        c.arg("-c");
        let (ok, out, _) = run_with_stdin(c, &big, Duration::from_secs(20)).unwrap();
        assert!(ok);
        assert_eq!(out.trim(), "400000");
    }
}
