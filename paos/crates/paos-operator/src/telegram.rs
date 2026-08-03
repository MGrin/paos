//! Telegram transport.
//!
//! **Shells out to `curl` rather than linking a TLS stack.** That is a deliberate
//! trade: rustls + ring + webpki + an HTTP client is roughly 30 crates and several MB
//! for a single long-poll every 25 seconds. `curl` ships with macOS, so the cost is one
//! subprocess per poll — utterly irrelevant at this rate — and the binary stays small
//! with no TLS supply chain to track.
//!
//! It also fixes something the Python got wrong: it opened a **fresh TCP+TLS connection
//! every 25 s, forever** (144 handshakes/hour, waking the radio each time). `--max-time`
//! plus a 50 s long-poll halves the wakeups outright.

use std::process::Command;

/// Telegram's maximum long-poll. The Python used 25 s, doubling the connection count
/// for no benefit.
pub const LONG_POLL_SECS: u32 = 50;

#[derive(Debug, Clone)]
pub struct Config {
    pub token: String,
    /// The GROUP chat. Messages belong in the group's per-room topics, never in a
    /// direct message to the bot — the group is where the rooms live.
    pub chat_id: String,
    /// True when `chat_id` is a forum group, so sends should carry a topic thread id.
    pub is_group: bool,
    /// The operator's real Telegram @username, so `@operator` becomes a genuine
    /// mention that actually pings them.
    pub operator_username: Option<String>,
    /// The ONLY user id allowed to drive the bus through this bridge.
    pub allowed_user_id: Option<i64>,
}

impl Config {
    /// Is this sender the operator?
    ///
    /// Fails CLOSED: an update with no sender id (a service message, a channel post, a
    /// bot) is not authorised. Without this gate anyone in the group can post to the bus
    /// as the `operator` identity, which the peer protocol treats as unforgeable.
    pub fn is_authorized(&self, from_id: Option<i64>) -> bool {
        match (self.allowed_user_id, from_id) {
            (Some(allowed), Some(got)) => allowed == got,
            // No allow-list configured means single-user machine; still require a sender.
            (None, Some(_)) => true,
            _ => false,
        }
    }
}

impl Config {
    /// Read credentials from the environment, else the skill's `.env`.
    ///
    /// Returns None when unconfigured — an unconfigured machine must run the bus and
    /// memory perfectly happily with no Telegram at all.
    /// Configuration from the database, falling back to the environment and `.env`.
    ///
    /// The database is FIRST so a fresh install configures itself through `paos init` and
    /// the settings page. The fallback is not legacy support to be removed later — it is
    /// what keeps an existing machine running through the change, and what makes a
    /// container deployment (env vars, no wizard) possible at all.
    ///
    /// The token is RESOLVED, never read from the row: the row holds a reference.
    pub fn from_db_or_env(conn: &rusqlite::Connection, env_path: &std::path::Path)
        -> Option<Config>
    {
        let get = |k: &str| -> Option<String> {
            conn.query_row("SELECT value FROM paos_config WHERE key=?1", [k], |r| r.get(0))
                .ok()
                .filter(|v: &String| !v.trim().is_empty())
        };
        let env = |n: &str| std::env::var(n).ok();
        let token = match get("telegram_bot_token") {
            Some(reference) => match paos_secrets::resolve(&reference, &env) {
                Some(t) => t,
                // A reference that resolves to nothing is NOT a fallback case: it means
                // someone configured a secret and the backend cannot produce it. Falling
                // through to the .env here would silently run on a different token than
                // the one they set, which is the hardest kind of wrong to notice.
                None => return None,
            },
            // No reference at all means this machine has not been configured through the
            // database, so hand over rather than half-configuring from two sources.
            None => return Config::from_env_or_file(env_path),
        };
        // The group wins when set; the bare user id is the last resort, exactly as in the
        // file path above — a machine with no group still has to reach its human.
        let chat_id = get("telegram_chat_id").or_else(|| get("telegram_allowed_user_id"))?;
        let is_group = chat_id.starts_with('-');
        Some(Config {
            token,
            chat_id,
            is_group,
            operator_username: get("telegram_operator_username")
                .map(|u| u.trim().trim_start_matches('@').to_string())
                .filter(|u| !u.is_empty()),
            allowed_user_id: get("telegram_allowed_user_id")
                .and_then(|v| v.trim().parse().ok()),
        })
    }

    pub fn from_env_or_file(env_path: &std::path::Path) -> Option<Config> {
        // Collect BOTH candidates, then choose. The previous version assigned to a
        // single `chat` on a first-wins basis while walking the file, so whichever key
        // happened to appear first won — and TELEGRAM_ALLOWED_USER_ID is listed before
        // TELEGRAM_GROUP_CHAT_ID in the real .env, so every message went to the bot's
        // direct chat instead of the group. File order is not a configuration choice.
        let mut token = std::env::var("TELEGRAM_BOT_TOKEN").ok();
        let mut group = std::env::var("TELEGRAM_GROUP_CHAT_ID").ok();
        let mut dm = std::env::var("TELEGRAM_ALLOWED_USER_ID").ok();
        let mut user = std::env::var("TELEGRAM_OPERATOR_USERNAME").ok();
        if let Ok(text) = std::fs::read_to_string(env_path) {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let Some((k, v)) = line.split_once('=') else { continue };
                let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
                if v.is_empty() {
                    continue;
                }
                match k.trim() {
                    "TELEGRAM_BOT_TOKEN" if token.is_none() => token = Some(v),
                    "TELEGRAM_GROUP_CHAT_ID" if group.is_none() => group = Some(v),
                    "TELEGRAM_ALLOWED_USER_ID" if dm.is_none() => dm = Some(v),
                    "TELEGRAM_OPERATOR_USERNAME" if user.is_none() => user = Some(v),
                    _ => {}
                }
            }
        }
        let token = token.filter(|t| !t.is_empty())?;
        // The group ALWAYS wins when configured; the DM is only a last resort for a
        // machine that has no group at all.
        let operator_username = user
            .map(|u| u.trim().trim_start_matches('@').to_string())
            .filter(|u| !u.is_empty());
        let allowed_user_id = dm.as_deref().and_then(|d| d.trim().parse::<i64>().ok());
        match group.filter(|g| !g.is_empty()) {
            Some(g) => Some(Config {
                token, chat_id: g, is_group: true, operator_username, allowed_user_id,
            }),
            None => dm.filter(|d| !d.is_empty()).map(|d| Config {
                token, chat_id: d, is_group: false, operator_username, allowed_user_id,
            }),
        }
    }
}

/// POST to the Bot API via curl. Returns the raw response body.
pub fn call(cfg: &Config, method: &str, fields: &[(&str, &str)], timeout: u32) -> Result<String, String> {
    let url = format!("https://api.telegram.org/bot{}/{}", cfg.token, method);
    let mut cmd = Command::new("curl");
    cmd.arg("-sS")
        .arg("--max-time").arg(timeout.to_string())
        .arg("-X").arg("POST");
    for (k, v) in fields {
        // --data-urlencode so a message body containing & or = cannot corrupt the
        // request. The Python's shell-quoted send path is exactly how bodies got mangled.
        cmd.arg("--data-urlencode").arg(format!("{k}={v}"));
    }
    cmd.arg(&url);
    let out = cmd.output().map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "curl exited {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Seconds Telegram asked us to wait, if it returned 429.
///
/// Ignoring this is how a rate-limited sender turns into a hot loop that stays
/// rate-limited forever.
pub fn retry_after(body: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("parameters")?
        .get("retry_after")?
        .as_u64()
}

/// Telegram rejects messages over 4096 characters. Leave headroom for the prefix.
pub const TG_LIMIT: usize = 3900;

/// Split long text on line boundaries where possible.
///
/// Without this, ONE long bus message 400s forever and wedges all delivery.
pub fn chunk_text(text: &str, limit: usize) -> Vec<String> {
    if text.chars().count() <= limit {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in text.split_inclusive('\n') {
        if line.chars().count() > limit {
            // A single enormous line: hard-split it, respecting char boundaries.
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            let mut buf = String::new();
            for ch in line.chars() {
                if buf.chars().count() + 1 > limit {
                    out.push(std::mem::take(&mut buf));
                }
                buf.push(ch);
            }
            if !buf.is_empty() {
                cur = buf;
            }
            continue;
        }
        if cur.chars().count() + line.chars().count() > limit {
            out.push(std::mem::take(&mut cur));
        }
        cur.push_str(line);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Send a message, optionally into a forum topic.
///
/// `thread_id` routes into the group's per-room topic. Without it everything piles into
/// General, which is what the rooms exist to avoid.
pub fn send(
    cfg: &Config,
    text: &str,
    silent: bool,
    thread_id: Option<i64>,
) -> Result<Option<i64>, String> {
    send_with_markup(cfg, text, silent, thread_id, None)
}

/// Send, optionally with an inline keyboard. Returns the LAST chunk's message_id, which
/// is what a reply would quote.
pub fn send_with_markup(
    cfg: &Config,
    text: &str,
    silent: bool,
    thread_id: Option<i64>,
    reply_markup: Option<&str>,
) -> Result<Option<i64>, String> {
    send_to(cfg, None, text, silent, thread_id, reply_markup)
}

/// Send to an explicit chat, falling back to the configured one.
///
/// `chat` is how a reply reaches the chat the command was typed in rather than always
/// the configured group. A topic thread only exists in the forum group, so a thread id is
/// dropped when the destination is anywhere else — sending one to a DM is an API error,
/// and the reply would vanish.
pub fn send_to(
    cfg: &Config,
    chat: Option<&str>,
    text: &str,
    silent: bool,
    thread_id: Option<i64>,
    reply_markup: Option<&str>,
) -> Result<Option<i64>, String> {
    let dest = chat.unwrap_or(&cfg.chat_id).to_string();
    let thread_id = if dest == cfg.chat_id { thread_id } else { None };
    let tid = thread_id.map(|t| t.to_string());
    let chunks = chunk_text(text, TG_LIMIT);
    let last = chunks.len().saturating_sub(1);
    let mut message_id = None;
    for (i, chunk) in chunks.iter().enumerate() {
        let mut fields: Vec<(&str, &str)> = vec![("chat_id", &dest), ("text", chunk)];
        if silent {
            fields.push(("disable_notification", "true"));
        }
        if let Some(t) = tid.as_deref() {
            fields.push(("message_thread_id", t));
        }
        // Buttons ride on the final chunk, where the reader ends up.
        if i == last {
            if let Some(m) = reply_markup {
                fields.push(("reply_markup", m));
            }
        }
        let mut attempt = 0;
        loop {
            let body = call(cfg, "sendMessage", &fields, 20)?;
            if body.contains("\"ok\":true") {
                message_id = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v.get("result")?.get("message_id")?.as_i64());
                break;
            }
            // Honour Telegram's own back-off rather than hammering it.
            if let Some(wait) = retry_after(&body) {
                if attempt < 3 {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_secs(wait.min(30) + 1));
                    continue;
                }
            }
            return Err(format!(
                "sendMessage rejected: {}",
                body.chars().take(200).collect::<String>()
            ));
        }
    }
    Ok(message_id)
}

/// Acknowledge an inline-button tap so the client stops spinning.
pub fn answer_callback(cfg: &Config, callback_id: &str, text: &str) {
    let _ = call(cfg, "answerCallbackQuery",
                 &[("callback_query_id", callback_id), ("text", text)], 10);
}

/// Replace a message's text — used to stamp an answered escalation, as an audit trail.
/// Edit a message and keep (or replace) its buttons.
///
/// `editMessageText` DROPS the inline keyboard unless reply_markup is sent again — so a
/// panel that refreshes itself would become a dead message after one tap.
pub fn edit_message_with_markup(cfg: &Config, message_id: i64, text: &str, markup: Option<&str>) {
    edit_message_in(cfg, None, message_id, text, markup)
}

/// Edit a message in the chat it actually lives in.
///
/// Replies already answer where the question was asked. Taps did not: a `message_id` is
/// only meaningful WITHIN a chat, so editing by id against the configured group while the
/// button was tapped in a direct message either fails or — worse — rewrites whichever
/// unrelated group message happens to hold that id. The panel and every digest button
/// work by editing in place, so this is the difference between them working in a DM and
/// doing nothing at all there.
pub fn edit_message_in(
    cfg: &Config,
    chat: Option<&str>,
    message_id: i64,
    text: &str,
    markup: Option<&str>,
) {
    let mid = message_id.to_string();
    let dest = chat.unwrap_or(&cfg.chat_id).to_string();
    let mut args: Vec<(&str, &str)> = vec![
        ("chat_id", &dest), ("message_id", &mid), ("text", text),
    ];
    if let Some(m) = markup {
        args.push(("reply_markup", m));
    }
    let _ = call(cfg, "editMessageText", &args, 10);
}

pub fn edit_message(cfg: &Config, message_id: i64, text: &str) {
    edit_message_in(cfg, None, message_id, text, None)
}

/// Publish the slash-command menu so the bot is discoverable.
///
/// Registered for the DEFAULT scope *and* explicitly for the operator's group. Telegram
/// resolves the `/` menu per scope, and a default-only registration is why the commands
/// showed up when talking to the bot privately but were unreliable in the group — which
/// is where the operator actually works. Everything the bot can do should be available
/// where the conversation happens.
pub fn set_my_commands(cfg: &Config, commands: &[(&str, &str)]) {
    let json: String = format!(
        "[{}]",
        commands.iter()
            .map(|(c, d)| format!("{{\"command\":\"{c}\",\"description\":\"{d}\"}}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    let _ = call(cfg, "setMyCommands", &[("commands", &json)], 10);
    if cfg.is_group {
        let scope = format!("{{\"type\":\"chat\",\"chat_id\":{}}}", cfg.chat_id);
        let _ = call(cfg, "setMyCommands",
                     &[("commands", &json), ("scope", &scope)], 10);
        // NOT setChatMenuButton: Telegram supports the ☰ menu button only in PRIVATE
        // chats — in a group it returns "invalid chat_id specified". Verified against the
        // API rather than assumed. In a group the chat-scoped list above is what makes
        // the commands appear when you type "/", which is the equivalent affordance.
    }
}

/// Inline keyboard JSON for an escalation's tap-to-answer options.
pub fn options_markup(id: i64, options: &[String]) -> Option<String> {
    if options.is_empty() {
        return None;
    }
    let buttons: Vec<String> = options.iter().enumerate()
        .map(|(i, o)| format!(
            "[{{\"text\":{},\"callback_data\":\"esc:{id}:{i}\"}}]",
            serde_json::to_string(o).unwrap_or_else(|_| "\"?\"".into())
        ))
        .collect();
    Some(format!("{{\"inline_keyboard\":[{}]}}", buttons.join(",")))
}

/// Rewrite `@handle` references so they read correctly on Telegram.
///
/// A leading `@` is a user-mention there, so a session handle like `@swift-otter` —
/// which is not a Telegram user — renders as a broken grey mention. Meanwhile
/// `@operator` SHOULD ping the actual human. So:
///   * `@operator` → `@<their real username>`, a genuine mention, else the bare word
///   * every other `@handle` → the bare handle, `@` dropped
///
/// `/` is deliberately untouched: sessions legitimately send file paths like
/// `/app/clients`, and turning those into commands would be worse than the problem.
pub fn neutralize_mentions(text: &str, operator_username: Option<&str>) -> String {
    let b: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != '@' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        // An '@' only starts a mention at a word boundary — otherwise an email address
        // would be mangled.
        let boundary = i == 0 || !(b[i - 1].is_alphanumeric() || b[i - 1] == '.' || b[i - 1] == '_');
        let mut j = i + 1;
        while j < b.len() && (b[j].is_alphanumeric() || b[j] == '_' || b[j] == '-') {
            j += 1;
        }
        if !boundary || j == i + 1 {
            out.push('@');
            i += 1;
            continue;
        }
        let handle: String = b[i + 1..j].iter().collect();
        if handle == "operator" {
            match operator_username {
                Some(u) => {
                    out.push('@');
                    out.push_str(u);
                }
                None => out.push_str("operator"),
            }
        } else {
            out.push_str(&handle);
        }
        i = j;
    }
    out
}

/// Reopen a closed forum topic. Cheaper and less confusing than making a new one with
/// the same name, which is how duplicates accumulate.
pub fn reopen_topic(cfg: &Config, thread_id: i64) -> Result<String, String> {
    let tid = thread_id.to_string();
    call(cfg, "reopenForumTopic",
         &[("chat_id", &cfg.chat_id), ("message_thread_id", &tid)], 10)
}

/// Retitle an existing forum topic.
///
/// A topic's name is set once at creation and then never revisited, so a room that later
/// declares which repos it is about keeps a title that does not say. The operator reads
/// this list on a phone to decide which topic to open; a title that omits the project is
/// the difference between a navigable group and a wall of names.
pub fn rename_topic(cfg: &Config, thread_id: i64, name: &str) -> Result<String, String> {
    let tid = thread_id.to_string();
    call(cfg, "editForumTopic",
         &[("chat_id", &cfg.chat_id), ("message_thread_id", &tid), ("name", name)], 10)
}

/// Create a forum topic and return its thread id.
pub fn create_topic(cfg: &Config, name: &str) -> Result<i64, String> {
    let body = call(cfg, "createForumTopic", &[("chat_id", &cfg.chat_id), ("name", name)], 20)?;
    body.split("\"message_thread_id\":")
        .nth(1)
        .and_then(|t| t.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok())
        .ok_or_else(|| format!("createForumTopic: {}", body.chars().take(200).collect::<String>()))
}

/// One inbound update, parsed properly.
///
/// The previous version scanned for the first `"text":"` in the payload. Telegram
/// serialises `reply_to_message` BEFORE `text`, so every quote-reply extracted the
/// **quoted** message's text and re-posted it to the bus as if the operator had typed
/// it. It also could not see `from.id`, so there was no way to authorise anything.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Update {
    pub update_id: i64,
    /// Sender's Telegram user id — the authorisation key.
    pub from_id: Option<i64>,
    pub text: Option<String>,
    /// Forum topic the message was sent in, which is how a room is addressed.
    pub thread_id: Option<i64>,
    /// message_id of the message being quoted, for reply routing.
    pub reply_to_message_id: Option<i64>,
    /// `callback_data` from an inline button tap.
    pub callback_data: Option<String>,
    /// The callback query id, needed to acknowledge the tap.
    pub callback_id: Option<String>,
    /// message_id of the message carrying the tapped button, so it can be edited.
    pub callback_message_id: Option<i64>,
    /// The sender's Telegram @username, so first-run setup can record it without asking.
    pub username: Option<String>,
    /// Which chat this arrived in.
    ///
    /// Absent from the struct until 2026-08-02, which meant every reply went to the
    /// CONFIGURED chat regardless of where the command was typed. A command sent in the
    /// bot DM ran correctly and answered into the group, so from the DM the bot looked
    /// completely dead — "no commands are working at all".
    pub chat_id: Option<String>,
}

fn as_i64(v: &serde_json::Value) -> Option<i64> {
    v.as_i64()
}

/// Parse a getUpdates response body into updates.
///
/// Malformed input yields an empty list rather than panicking: this parses whatever a
/// remote service sends us.
pub fn parse_updates(body: &str) -> Vec<Update> {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let Some(items) = root.get("result").and_then(|r| r.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for it in items {
        let mut u = Update {
            update_id: it.get("update_id").and_then(as_i64).unwrap_or(0),
            ..Default::default()
        };
        if let Some(cq) = it.get("callback_query") {
            u.from_id = cq.get("from").and_then(|f| f.get("id")).and_then(as_i64);
            u.callback_data = cq.get("data").and_then(|d| d.as_str()).map(str::to_string);
            u.callback_id = cq.get("id").and_then(|d| d.as_str()).map(str::to_string);
            u.callback_message_id =
                cq.get("message").and_then(|m| m.get("message_id")).and_then(as_i64);
            u.chat_id = cq
                .get("message")
                .and_then(|m| m.get("chat"))
                .and_then(|c| c.get("id"))
                .and_then(as_i64)
                .map(|i| i.to_string());
            out.push(u);
            continue;
        }
        let msg = it
            .get("message")
            .or_else(|| it.get("edited_message"))
            .or_else(|| it.get("channel_post"));
        if let Some(m) = msg {
            u.from_id = m.get("from").and_then(|f| f.get("id")).and_then(as_i64);
            // `text` read from the MESSAGE object, not the first one in the payload.
            u.text = m.get("text").and_then(|t| t.as_str()).map(str::to_string);
            u.thread_id = m.get("message_thread_id").and_then(as_i64);
            u.reply_to_message_id = m
                .get("reply_to_message")
                .and_then(|r| r.get("message_id"))
                .and_then(as_i64);
            u.chat_id = m
                .get("chat")
                .and_then(|c| c.get("id"))
                .and_then(as_i64)
                .map(|i| i.to_string());
            u.username = m
                .get("from")
                .and_then(|f| f.get("username"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
        }
        out.push(u);
    }
    out
}

/// What the first message to the bot tells us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub chat_id: String,
    pub user_id: i64,
    pub username: Option<String>,
}

/// Learn the ids from a real MESSAGE.
///
/// Nobody should have to find a numeric Telegram id by hand — the bot is told them by
/// the only person who can prove they own the chat: whoever speaks in it.
///
/// Deliberately not from a callback. A tap carries a chat id, but the wizard is asking
/// someone to prove they own the conversation by speaking in it, and tapping a button
/// someone else posted is not that.
pub fn discovered_from(updates: &[Update]) -> Option<Discovered> {
    updates.iter().find_map(|u| {
        if u.callback_data.is_some() {
            return None;
        }
        Some(Discovered {
            chat_id: u.chat_id.clone()?,
            user_id: u.from_id?,
            username: u.username.clone(),
        })
    })
}

/// Highest update_id seen, for advancing the offset.
pub fn max_update_id(updates: &[Update]) -> Option<i64> {
    updates.iter().map(|u| u.update_id).filter(|i| *i > 0).max()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unconfigured_machine_yields_no_config() {
        // PAOS must run fine with no Telegram at all.
        assert!(Config::from_env_or_file(std::path::Path::new("/definitely/not/here")).is_none());
    }

    #[test]
    fn reads_credentials_from_an_env_file() {
        let dir = std::env::temp_dir().join(format!("paos-tg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(".env");
        std::fs::write(&f, "# comment\nTELEGRAM_BOT_TOKEN=\"123:abc\"\nTELEGRAM_ALLOWED_USER_ID=42\n").unwrap();
        let cfg = Config::from_env_or_file(&f).expect("should parse");
        assert_eq!(cfg.token, "123:abc");
        assert_eq!(cfg.chat_id, "42");
        assert!(!cfg.is_group, "a bare user id is a direct chat, not a forum");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn configuration_comes_from_the_database_first() {
        let c = paos_store::open_in_memory().unwrap();
        for (k, v) in [("telegram_bot_token", "env:PAOS_TEST_TG_TOKEN"),
                       ("telegram_chat_id", "-100777"),
                       ("telegram_allowed_user_id", "7"),
                       ("telegram_operator_username", "@someone")] {
            c.execute("INSERT INTO paos_config(key,value,updated_ts) VALUES(?1,?2,'t')",
                      rusqlite::params![k, v]).unwrap();
        }
        std::env::set_var("PAOS_TEST_TG_TOKEN", "123:abc");
        let cfg = Config::from_db_or_env(&c, std::path::Path::new("/definitely/not/here"))
            .expect("configured from the database");
        std::env::remove_var("PAOS_TEST_TG_TOKEN");
        assert_eq!(cfg.token, "123:abc", "the token is RESOLVED, not read from the row");
        assert_eq!(cfg.chat_id, "-100777");
        assert_eq!(cfg.allowed_user_id, Some(7));
        assert_eq!(cfg.operator_username.as_deref(), Some("someone"),
                   "stored with or without the @, used without");
        assert!(cfg.is_group, "a negative chat id is a group");
    }

    #[test]
    fn an_unconfigured_database_falls_back_to_the_env_file() {
        // The maintainer's own machine keeps working through the cutover: nothing has
        // written these rows yet, and the .env is still there.
        let c = paos_store::open_in_memory().unwrap();
        let dir = std::env::temp_dir().join(format!("paos-tg-db-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(".env");
        std::fs::write(&f, "TELEGRAM_BOT_TOKEN=\"9:z\"\nTELEGRAM_ALLOWED_USER_ID=42\n").unwrap();
        let cfg = Config::from_db_or_env(&c, &f).expect("falls back");
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(cfg.token, "9:z");
    }

    #[test]
    fn a_reference_that_resolves_to_nothing_is_not_a_config() {
        // Starting the bridge with an empty token produces 401s on every poll and no
        // explanation. No configuration at all is the honest state.
        let c = paos_store::open_in_memory().unwrap();
        c.execute("INSERT INTO paos_config(key,value,updated_ts) \
                   VALUES('telegram_bot_token','env:PAOS_TEST_ABSENT','t')", []).unwrap();
        assert!(Config::from_db_or_env(&c, std::path::Path::new("/definitely/not/here"))
                .is_none());
    }

    #[test]
    fn the_first_message_teaches_us_the_chat_and_the_user() {
        // Nobody should have to find a numeric Telegram id by hand. The bot is told them
        // by the only person who can prove they own the chat: whoever messages it.
        let body = r#"{"ok":true,"result":[{"update_id":1,"message":{
            "from":{"id":7,"username":"someone"},"chat":{"id":-100777},"text":"hi"}}]}"#;
        let d = discovered_from(&parse_updates(body)).expect("learned");
        assert_eq!(d.chat_id, "-100777");
        assert_eq!(d.user_id, 7);
        assert_eq!(d.username.as_deref(), Some("someone"));
    }

    #[test]
    fn a_button_tap_does_not_teach_us_anything() {
        // A callback has no author we can trust as "the operator introducing themselves";
        // the wizard must wait for a real message.
        let body = r#"{"ok":true,"result":[{"update_id":2,"callback_query":{
            "id":"cb","from":{"id":7},"data":"panel:who","message":{"message_id":9,
            "chat":{"id":-100777}}}}]}"#;
        assert!(discovered_from(&parse_updates(body)).is_none());
    }

    #[test]
    fn nothing_to_learn_from_an_empty_poll() {
        assert!(discovered_from(&[]).is_none());
    }

    #[test]
    fn a_message_from_a_user_with_no_username_still_teaches_the_ids() {
        // A Telegram account need not have a @username. Requiring one would strand
        // exactly the people who never set one.
        let body = r#"{"ok":true,"result":[{"update_id":3,"message":{
            "from":{"id":7},"chat":{"id":7},"text":"hi"}}]}"#;
        let d = discovered_from(&parse_updates(body)).expect("learned");
        assert_eq!(d.user_id, 7);
        assert_eq!(d.username, None);
    }

    #[test]
    fn parses_ids_text_and_the_highest_offset() {
        let body = r#"{"ok":true,"result":[
            {"update_id":10,"message":{"from":{"id":7},"text":"hello"}},
            {"update_id":11,"message":{"from":{"id":7},"text":"second"}}]}"#;
        let u = parse_updates(body);
        assert_eq!(u.len(), 2);
        assert_eq!(u[0].text.as_deref(), Some("hello"));
        assert_eq!(u[0].from_id, Some(7));
        assert_eq!(max_update_id(&u), Some(11), "or updates repeat forever");
    }

    #[test]
    fn a_quote_reply_reads_the_new_text_not_the_quoted_one() {
        // REGRESSION: the old scanner took the first `"text":"` in the payload, and
        // Telegram serialises reply_to_message BEFORE text — so every quote-reply
        // re-posted the QUOTED message to the bus as if the operator had typed it.
        let body = r#"{"ok":true,"result":[{"update_id":1,"message":{
            "from":{"id":7},
            "reply_to_message":{"message_id":42,"text":"THE OLD QUOTED TEXT"},
            "text":"my actual reply"}}]}"#;
        let u = parse_updates(body);
        assert_eq!(u[0].text.as_deref(), Some("my actual reply"));
        assert_eq!(u[0].reply_to_message_id, Some(42), "needed to route the reply");
    }

    #[test]
    fn reads_the_sender_id_so_inbound_can_be_authorised() {
        // Without this there is no authorisation at all, and anyone in the group can
        // post to the bus as the `operator` identity.
        let u = parse_updates(r#"{"result":[{"update_id":1,"message":{"from":{"id":8144244025},"text":"hi"}}]}"#);
        assert_eq!(u[0].from_id, Some(8_144_244_025));
    }

    #[test]
    fn reads_the_forum_topic_so_a_room_can_be_addressed() {
        let u = parse_updates(r#"{"result":[{"update_id":1,"message":{"from":{"id":7},"message_thread_id":195,"text":"x"}}]}"#);
        assert_eq!(u[0].thread_id, Some(195));
    }

    #[test]
    fn parses_an_inline_button_tap() {
        let body = r#"{"result":[{"update_id":9,"callback_query":{
            "id":"cb1","from":{"id":7},"data":"esc:12:1",
            "message":{"message_id":77}}}]}"#;
        let u = parse_updates(body);
        assert_eq!(u[0].callback_data.as_deref(), Some("esc:12:1"));
        assert_eq!(u[0].callback_id.as_deref(), Some("cb1"));
        assert_eq!(u[0].callback_message_id, Some(77));
    }

    #[test]
    fn broken_payloads_yield_nothing_rather_than_panicking() {
        for body in ["", "{}", r#"{"ok":false}"#, "garbage", r#"{"result":"nope"}"#] {
            assert!(parse_updates(body).is_empty(), "unexpected updates from {body:?}");
        }
    }

    #[test]
    fn long_text_is_chunked_on_line_boundaries() {
        // THE WEDGE: one >4096-char message used to 400 forever and stop ALL delivery.
        let text = (0..200).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let chunks = chunk_text(&text, 100);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.chars().count() <= 100), "a chunk exceeded the limit");
        assert_eq!(chunks.concat(), text, "chunking must be lossless");
    }

    #[test]
    fn a_single_enormous_line_is_still_split() {
        let text = "x".repeat(5000);
        let chunks = chunk_text(&text, 100);
        assert!(chunks.iter().all(|c| c.chars().count() <= 100));
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn chunking_respects_char_boundaries_on_multibyte_text() {
        let text = "é".repeat(300);
        let chunks = chunk_text(&text, 100);
        assert_eq!(chunks.concat(), text, "must not split inside a character");
    }

    #[test]
    fn short_text_is_one_chunk() {
        assert_eq!(chunk_text("hello", 100), vec!["hello"]);
    }

    #[test]
    fn retry_after_is_read_from_a_429_body() {
        // Ignoring this turns a rate-limited sender into a hot loop.
        assert_eq!(retry_after(r#"{"ok":false,"parameters":{"retry_after":17}}"#), Some(17));
        assert_eq!(retry_after(r#"{"ok":true}"#), None);
        assert_eq!(retry_after("garbage"), None);
    }

    #[test]
    fn options_render_as_tap_to_answer_buttons() {
        let m = options_markup(12, &["ship now".into(), "hold".into()]).unwrap();
        assert!(m.contains("esc:12:0") && m.contains("esc:12:1"), "{m}");
        assert!(m.contains("ship now"));
        assert!(options_markup(12, &[]).is_none(), "no options, no keyboard");
    }

    #[test]
    fn long_poll_is_telegrams_maximum() {
        // The Python used 25s, doubling TCP+TLS handshakes for no benefit — 144/hour,
        // each waking the radio.
        assert_eq!(LONG_POLL_SECS, 50);
    }

    #[test]
    fn the_group_wins_even_when_listed_after_the_dm() {
        // REGRESSION: the parser assigned first-wins while walking the file, and the
        // real .env lists TELEGRAM_ALLOWED_USER_ID BEFORE TELEGRAM_GROUP_CHAT_ID — so
        // every message went to the bot's direct chat instead of the group where the
        // rooms live. File order is not a configuration choice.
        let dir = std::env::temp_dir().join(format!("paos-tg-order-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(".env");
        std::fs::write(&f, "TELEGRAM_BOT_TOKEN=t\nTELEGRAM_ALLOWED_USER_ID=42\nTELEGRAM_GROUP_CHAT_ID=-100999\n").unwrap();
        let cfg = Config::from_env_or_file(&f).unwrap();
        assert_eq!(cfg.chat_id, "-100999", "the group must win regardless of file order");
        assert!(cfg.is_group);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_group_id_falls_back_to_the_dm() {
        let dir = std::env::temp_dir().join(format!("paos-tg-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(".env");
        std::fs::write(&f, "TELEGRAM_BOT_TOKEN=t\nTELEGRAM_GROUP_CHAT_ID=\nTELEGRAM_ALLOWED_USER_ID=42\n").unwrap();
        let cfg = Config::from_env_or_file(&f).unwrap();
        assert_eq!(cfg.chat_id, "42");
        assert!(!cfg.is_group);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn operator_mention_becomes_the_real_username() {
        // The whole point: @operator must actually ping the human.
        assert_eq!(
            neutralize_mentions("ping @operator now", Some("example_operator")),
            "ping @example_operator now"
        );
    }

    #[test]
    fn peer_handles_lose_the_at_so_they_are_not_broken_mentions() {
        assert_eq!(
            neutralize_mentions("@swift-otter replied to @quiet-bison", None),
            "swift-otter replied to quiet-bison"
        );
    }

    #[test]
    fn without_a_configured_username_operator_degrades_to_a_plain_word() {
        assert_eq!(neutralize_mentions("tell @operator", None), "tell operator");
    }

    #[test]
    fn file_paths_are_left_alone() {
        // Sessions send paths constantly; mangling them would be worse than the bug.
        let s = "see /app/clients and /Users/example/.paos/paos.db";
        assert_eq!(neutralize_mentions(s, Some("example_operator")), s);
    }

    #[test]
    fn email_addresses_are_not_treated_as_mentions() {
        let s = "mail third@example.com about it";
        assert_eq!(neutralize_mentions(s, Some("example_operator")), s);
    }

    #[test]
    fn a_bare_at_sign_survives() {
        assert_eq!(neutralize_mentions("cost @ 5 usd", Some("u")), "cost @ 5 usd");
        assert_eq!(neutralize_mentions("@", Some("u")), "@");
    }

    #[test]
    fn multi_recipient_targets_are_handled_per_handle() {
        assert_eq!(
            neutralize_mentions("@alice__bob and @operator", Some("example_operator")),
            "alice__bob and @example_operator"
        );
    }

    #[test]
    fn operator_username_is_read_from_the_env_file_without_its_at() {
        let dir = std::env::temp_dir().join(format!("paos-tg-user-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(".env");
        std::fs::write(&f, "TELEGRAM_BOT_TOKEN=t\nTELEGRAM_GROUP_CHAT_ID=-100\nTELEGRAM_OPERATOR_USERNAME=@example_operator\n").unwrap();
        let cfg = Config::from_env_or_file(&f).unwrap();
        assert_eq!(cfg.operator_username.as_deref(), Some("example_operator"), "the @ must be stripped once");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn only_the_operator_is_authorized() {
        // The Python daemon rejects user 8144244025 in this very group today; the Rust
        // bridge had no gate at all and would have posted that message AS the operator.
        let cfg = Config {
            token: "t".into(), chat_id: "-100".into(), is_group: true,
            operator_username: None, allowed_user_id: Some(494_668_677),
        };
        assert!(cfg.is_authorized(Some(494_668_677)));
        assert!(!cfg.is_authorized(Some(8_144_244_025)));
        assert!(!cfg.is_authorized(None), "a sender-less update must fail closed");
    }

    #[test]
    fn allowed_user_id_is_parsed_from_the_env() {
        let dir = std::env::temp_dir().join(format!("paos-tg-auth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(".env");
        std::fs::write(&f, "TELEGRAM_BOT_TOKEN=t\nTELEGRAM_ALLOWED_USER_ID=494668677\nTELEGRAM_GROUP_CHAT_ID=-100\n").unwrap();
        let cfg = Config::from_env_or_file(&f).unwrap();
        assert_eq!(cfg.allowed_user_id, Some(494_668_677));
        assert_eq!(cfg.chat_id, "-100", "still sends to the group");
        std::fs::remove_dir_all(&dir).ok();
    }
    /// The field whose absence made the bot look dead in a DM: every reply went to the
    /// configured group instead of the chat the command was typed in.
    #[test]
    fn an_update_records_which_chat_it_came_from() {
        let u = parse_updates(
            r#"{"result":[{"update_id":1,"message":{"from":{"id":7},"chat":{"id":-100123},"text":"/tasks"}}]}"#);
        assert_eq!(u[0].chat_id.as_deref(), Some("-100123"));

        let dm = parse_updates(
            r#"{"result":[{"update_id":2,"message":{"from":{"id":7},"chat":{"id":555},"text":"/tasks"}}]}"#);
        assert_eq!(dm[0].chat_id.as_deref(), Some("555"));
    }

    #[test]
    fn a_button_tap_records_its_chat_too() {
        let u = parse_updates(
            r#"{"result":[{"update_id":3,"callback_query":{"id":"cb","data":"task:done:t-a",
                 "message":{"message_id":9,"chat":{"id":555}}}}]}"#);
        assert_eq!(u[0].chat_id.as_deref(), Some("555"));
        assert_eq!(u[0].callback_data.as_deref(), Some("task:done:t-a"));
    }
}
