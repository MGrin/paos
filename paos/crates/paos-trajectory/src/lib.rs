//! Normalize local coding-agent sessions into a compact, memory-ready record format.
//!
//! Port of `trajectory_facet.py`. Inspired by Letta AI's `@letta-ai/trajectory`: agent
//! harnesses record the same concepts — messages, reasoning, tool calls, tool results —
//! in verbose, incompatible native formats. This crate reads this machine's Claude Code
//! session JSONL and normalizes it into one small shape designed to be *consumed for
//! memory formation* (`paos memory dream`, `paos memory lessons`). It drops harness
//! bookkeeping and truncates long tool output, yielding a large token reduction.
//!
//! Pure ETL: no SQLite, no daemon socket, no LLM, no network. `normalize_transcript`
//! never touches the filesystem.

pub mod episodes;
pub mod discovery;
pub mod json;

pub use episodes::{error_signature, failure_episodes, is_teachable, Episode};
pub use discovery::{default_root, list_trajectories, resolve_session, Item, Listing};

use serde::Serialize;

pub const SOURCES: &[&str] = &["claude-code"];
/// Chars per tool result; 0 disables.
pub const DEFAULT_TRUNCATE: usize = 2000;

/// One normalized record.
///
/// Serialized untagged so each variant emits its fields in the SAME ORDER the Python
/// dicts were built in. That order is not cosmetic: `paos trajectory show --json` is
/// diffed byte-for-byte against the Python during the port, and a reordered key is
/// indistinguishable from a dropped one in that diff.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Record {
    Meta(Meta),
    /// `user` and `reasoning` share a shape; `role` distinguishes them.
    Text(TextRecord),
    Assistant(Assistant),
    Tool(Tool),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Meta {
    pub role: &'static str,
    pub source: String,
    // These four are ALWAYS emitted, as null when absent — Python built them with
    // `meta.get(...)`, so the key is present with a null value. Skipping them would
    // change the JSON shape.
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub started_at: Option<String>,
    pub num_records: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TextRecord {
    pub role: &'static str,
    pub content: String,
    pub timestamp: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Assistant {
    pub role: &'static str,
    /// None when the turn was tool calls only.
    pub content: Option<String>,
    pub timestamp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolCall {
    pub id: Option<String>,
    pub name: Option<String>,
    pub args: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Tool {
    pub role: &'static str,
    pub tool_call_id: Option<String>,
    pub content: String,
    pub timestamp: Option<String>,
    /// Present only when true — Python set the key only on an error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Diagnostic {
    pub code: &'static str,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Normalized {
    pub records: Vec<Record>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Record {
    pub fn role(&self) -> &str {
        match self {
            Record::Meta(_) => "meta",
            Record::Text(t) => t.role,
            Record::Assistant(_) => "assistant",
            Record::Tool(_) => "tool",
        }
    }
}

/// Cap a string to `n` chars with a visible marker. `n == 0` leaves it unchanged.
///
/// Counts CHARACTERS, not bytes — Python slices by codepoint, and a byte slice would
/// both cut differently and risk splitting a multi-byte character.
pub fn truncate(s: &str, n: usize) -> String {
    if n == 0 {
        return s.to_string();
    }
    let total = s.chars().count();
    if total <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n).collect();
    format!("{head}\n…[truncated {} chars]", total - n)
}

/// Cheap token estimate (~4 chars/token) — for the stats headline only.
///
/// Python's `len(text) // 4` counts CODEPOINTS. `str::len()` would count bytes and
/// inflate the estimate for any transcript containing non-ASCII.
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count() / 4
}

/// Normalize a tool_use input to a stringified JSON object (Letta contract).
fn args_str(input: Option<&serde_json::Value>) -> String {
    match input {
        None | Some(serde_json::Value::Null) => "{}".to_string(),
        Some(serde_json::Value::String(s)) => {
            let t = s.trim();
            // Already valid JSON? Pass it through verbatim, exactly as Python did.
            if serde_json::from_str::<serde_json::Value>(t).is_ok() {
                t.to_string()
            } else {
                json::dumps_compact(&serde_json::Value::String(s.clone()))
            }
        }
        Some(v) => json::dumps_compact(v),
    }
}

/// Flatten a tool_result content (string | list of blocks) into plain text.
fn flatten_content(c: Option<&serde_json::Value>) -> String {
    match c {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(items)) => {
            let mut parts: Vec<String> = Vec::new();
            for b in items {
                match b {
                    serde_json::Value::Object(_) => {
                        let ty = b.get("type").and_then(|v| v.as_str());
                        if ty == Some("text") || b.get("text").is_some() {
                            parts.push(
                                b.get("text")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                            );
                        } else if ty == Some("image") {
                            parts.push("[image]".to_string());
                        } else {
                            // Python: json.dumps(b, ensure_ascii=False)[:200] — a CHAR
                            // slice, so cut by chars here too.
                            let dumped = json::dumps_compact(b);
                            parts.push(dumped.chars().take(200).collect());
                        }
                    }
                    serde_json::Value::String(s) => parts.push(s.clone()),
                    _ => {}
                }
            }
            parts
                .into_iter()
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        }
        Some(v) => v.to_string(),
    }
}

fn is_blank(s: &str) -> bool {
    s.trim().is_empty()
}

/// Split JSONL records on `\n`, and ONLY on `\n`.
///
/// Both this and the Python used to use their language's "split into lines" helper, and
/// Python's `str.splitlines()` breaks on eight more characters — VT, FF, FS, GS, RS,
/// NEL, U+2028, U+2029 — none of which separates JSONL records. A record whose text
/// merely CONTAINED one was cut in half: a valid line became two unparseable ones, the
/// record was dropped, and the only trace was a `bad_json` diagnostic nothing reads.
///
/// Measured before the fix: session 4b784b32 lost 5 lines to 2 stray U+2028 characters,
/// and these transcripts are the input to `dream` and `lessons` — so the loss was silent
/// exactly where it mattered. Fixed in both implementations in one commit, so the parity
/// diff stayed green and kept its meaning.
///
/// A trailing `\r` is handled by the caller's `trim()`, which is what makes this safe on
/// a CRLF transcript.
fn split_records(s: &str) -> impl Iterator<Item = &str> {
    s.split('\n')
}

/// The ETL core for Claude Code transcripts.
fn normalize_claude_code(transcript: &str, trunc: usize) -> Normalized {
    let mut records: Vec<Record> = Vec::new();
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut started_at: Option<String> = None;

    for (i, raw) in split_records(transcript).enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let o: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                diagnostics.push(Diagnostic { code: "bad_json", line: i + 1 });
                continue;
            }
        };
        if !o.is_object() {
            diagnostics.push(Diagnostic { code: "bad_json", line: i + 1 });
            continue;
        }

        // Session metadata from the first line that carries each field.
        if session_id.is_none() {
            if let Some(s) = o.get("sessionId").and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    session_id = Some(s.to_string());
                }
            }
        }
        if cwd.is_none() {
            if let Some(s) = o.get("cwd").and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    cwd = Some(s.to_string());
                }
            }
        }
        if git_branch.is_none() {
            if let Some(s) = o.get("gitBranch").and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    git_branch = Some(s.to_string());
                }
            }
        }

        let t = o.get("type").and_then(|v| v.as_str()).unwrap_or("");
        // Drop queue-operation / attachment / last-prompt / system / ...
        if t != "user" && t != "assistant" {
            continue;
        }

        let msg = o.get("message");
        let content = msg.and_then(|m| m.get("content"));
        let ts = o.get("timestamp").and_then(|v| v.as_str()).map(String::from);
        if ts.is_some() && started_at.is_none() {
            started_at = ts.clone();
        }

        if t == "user" {
            match content {
                Some(serde_json::Value::String(s)) => {
                    if !is_blank(s) {
                        records.push(Record::Text(TextRecord {
                            role: "user",
                            content: s.clone(),
                            timestamp: ts.clone(),
                        }));
                    }
                }
                Some(serde_json::Value::Array(blocks)) => {
                    let mut text_parts: Vec<String> = Vec::new();
                    for b in blocks {
                        if !b.is_object() {
                            continue;
                        }
                        match b.get("type").and_then(|v| v.as_str()) {
                            Some("tool_result") => {
                                let body = truncate(&flatten_content(b.get("content")), trunc);
                                // Keep the error flag. Dropping it normalized a failed
                                // Bash and a successful one to byte-identical records,
                                // erasing the only signal that says WHERE the session
                                // learned something.
                                let is_error = match b.get("is_error") {
                                    Some(v) if truthy(v) => Some(true),
                                    _ => None,
                                };
                                records.push(Record::Tool(Tool {
                                    role: "tool",
                                    tool_call_id: b
                                        .get("tool_use_id")
                                        .and_then(|v| v.as_str())
                                        .map(String::from),
                                    content: body,
                                    timestamp: ts.clone(),
                                    is_error,
                                }));
                            }
                            Some("text") => {
                                text_parts.push(
                                    b.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                                );
                            }
                            _ => {}
                        }
                    }
                    let joined = text_parts
                        .into_iter()
                        .filter(|p| !p.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !is_blank(&joined) {
                        records.push(Record::Text(TextRecord {
                            role: "user",
                            content: joined,
                            timestamp: ts.clone(),
                        }));
                    }
                }
                _ => {}
            }
            continue;
        }

        // assistant
        let mut text_parts: Vec<String> = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        match content {
            Some(serde_json::Value::String(s)) => {
                if !is_blank(s) {
                    text_parts.push(s.clone());
                }
            }
            Some(serde_json::Value::Array(blocks)) => {
                for b in blocks {
                    if !b.is_object() {
                        continue;
                    }
                    match b.get("type").and_then(|v| v.as_str()) {
                        Some("thinking") => {
                            let th = b.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                            if !is_blank(th) {
                                records.push(Record::Text(TextRecord {
                                    role: "reasoning",
                                    content: th.to_string(),
                                    timestamp: ts.clone(),
                                }));
                            }
                        }
                        Some("text") => {
                            text_parts.push(
                                b.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                            );
                        }
                        Some("tool_use") => {
                            tool_calls.push(ToolCall {
                                id: b.get("id").and_then(|v| v.as_str()).map(String::from),
                                name: b.get("name").and_then(|v| v.as_str()).map(String::from),
                                args: args_str(b.get("input")),
                            });
                        }
                        // `redacted_thinking` is an opaque blob — drop it.
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        let body = text_parts
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if !is_blank(&body) || !tool_calls.is_empty() {
            records.push(Record::Assistant(Assistant {
                role: "assistant",
                content: if is_blank(&body) { None } else { Some(body) },
                timestamp: ts.clone(),
                tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls) },
            }));
        }
    }

    if records.is_empty() {
        // No meta record when there is nothing to describe — Python returned early here.
        return Normalized { records: Vec::new(), diagnostics };
    }

    let meta = Record::Meta(Meta {
        role: "meta",
        source: "claude-code".to_string(),
        session_id,
        cwd,
        git_branch,
        started_at,
        num_records: records.len(),
    });
    let mut out = Vec::with_capacity(records.len() + 1);
    out.push(meta);
    out.extend(records);
    Normalized { records: out, diagnostics }
}

/// Python truthiness for the `is_error` flag: `false`, `0`, `""`, `[]`, `{}` and `null`
/// are all falsy. A plain `v.as_bool() == Some(true)` would treat `"is_error": 1` — which
/// some harness versions emit — as absent.
fn truthy(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(o) => !o.is_empty(),
    }
}

/// Parse a native session transcript into normalized trajectory records.
///
/// Never touches the filesystem. Errors for an unsupported source.
pub fn normalize_transcript(
    source: &str,
    transcript: &str,
    trunc: usize,
) -> Result<Normalized, String> {
    if source != "claude-code" {
        return Err(format!(
            "unsupported source '{source}' (supported: {})",
            SOURCES.join(", ")
        ));
    }
    Ok(normalize_claude_code(transcript, trunc))
}

/// Compact plain-text rendering of a normalized trajectory for LLM consumption.
pub fn render_text(records: &[Record], trunc: usize) -> String {
    let mut out: Vec<String> = Vec::new();
    for r in records {
        match r {
            Record::Meta(m) => out.push(format!(
                "[{} · {} · {} · {}]",
                m.source,
                m.session_id.as_deref().unwrap_or("?"),
                m.cwd.as_deref().unwrap_or("?"),
                m.git_branch.as_deref().unwrap_or("?"),
            )),
            Record::Text(t) if t.role == "user" => {
                out.push(format!("user: {}", truncate(&t.content, trunc)))
            }
            Record::Text(t) => out.push(format!("think: {}", truncate(&t.content, trunc))),
            Record::Assistant(a) => {
                if let Some(c) = a.content.as_deref().filter(|c| !c.is_empty()) {
                    out.push(format!("assistant: {}", truncate(c, trunc)));
                }
                for tc in a.tool_calls.as_deref().unwrap_or(&[]) {
                    out.push(format!(
                        "  ⏵ {}({})",
                        // Python formatted None as the literal "None" here.
                        tc.name.as_deref().unwrap_or("None"),
                        truncate(&tc.args, 300)
                    ));
                }
            }
            // Mark failures. Rendering an error identically to a success means the
            // distiller reading this cannot tell where the session got stuck — the single
            // most informative thing in a transcript.
            Record::Tool(t) => out.push(format!(
                "{}{}",
                if t.is_error == Some(true) { "tool ERROR: " } else { "tool: " },
                truncate(&t.content, trunc)
            )),
        }
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jsonl(lines: &[serde_json::Value]) -> String {
        lines.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("\n")
    }

    fn user(text: &str) -> serde_json::Value {
        serde_json::json!({"type":"user","message":{"content":text},
                           "sessionId":"s1","cwd":"/w","gitBranch":"main",
                           "timestamp":"2026-01-01T00:00:00Z"})
    }

    #[test]
    fn source_validated() {
        assert!(normalize_transcript("cursor", "", DEFAULT_TRUNCATE).is_err());
        assert!(normalize_transcript("claude-code", "", DEFAULT_TRUNCATE).is_ok());
    }

    #[test]
    fn meta_record_first() {
        let n = normalize_transcript("claude-code", &jsonl(&[user("hi")]), DEFAULT_TRUNCATE)
            .unwrap();
        match &n.records[0] {
            Record::Meta(m) => {
                assert_eq!(m.session_id.as_deref(), Some("s1"));
                assert_eq!(m.cwd.as_deref(), Some("/w"));
                assert_eq!(m.git_branch.as_deref(), Some("main"));
                assert_eq!(m.num_records, 1);
                assert_eq!(m.started_at.as_deref(), Some("2026-01-01T00:00:00Z"));
            }
            other => panic!("expected meta, got {other:?}"),
        }
    }

    #[test]
    fn roles_and_order() {
        let t = jsonl(&[
            user("hi"),
            serde_json::json!({"type":"assistant","message":{"content":[
                {"type":"thinking","thinking":"hmm"},
                {"type":"text","text":"hello"},
                {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}),
            serde_json::json!({"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"t1","content":"out"}]}}),
        ]);
        let n = normalize_transcript("claude-code", &t, DEFAULT_TRUNCATE).unwrap();
        let roles: Vec<&str> = n.records.iter().map(|r| r.role()).collect();
        assert_eq!(roles, vec!["meta", "user", "reasoning", "assistant", "tool"]);
    }

    #[test]
    fn assistant_tool_calls_carry_stringified_args() {
        let t = jsonl(&[serde_json::json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}})]);
        let n = normalize_transcript("claude-code", &t, DEFAULT_TRUNCATE).unwrap();
        match &n.records[1] {
            Record::Assistant(a) => {
                assert_eq!(a.content, None, "a tool-only turn has no text body");
                let tc = &a.tool_calls.as_ref().unwrap()[0];
                assert_eq!(tc.id.as_deref(), Some("t1"));
                // Spaced separators, because that is what json.dumps emits.
                assert_eq!(tc.args, r#"{"command": "ls"}"#);
            }
            other => panic!("expected assistant, got {other:?}"),
        }
    }

    #[test]
    fn an_error_result_is_marked_as_one() {
        let t = jsonl(&[serde_json::json!({"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"t1","content":"boom","is_error":true}]}})]);
        let n = normalize_transcript("claude-code", &t, DEFAULT_TRUNCATE).unwrap();
        match &n.records[1] {
            Record::Tool(tr) => assert_eq!(tr.is_error, Some(true)),
            other => panic!("expected tool, got {other:?}"),
        }
    }

    #[test]
    fn a_successful_result_is_not_marked() {
        let t = jsonl(&[serde_json::json!({"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"t1","content":"fine"}]}})]);
        let n = normalize_transcript("claude-code", &t, DEFAULT_TRUNCATE).unwrap();
        match &n.records[1] {
            Record::Tool(tr) => assert_eq!(tr.is_error, None),
            other => panic!("expected tool, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_list_content_flattened() {
        let t = jsonl(&[serde_json::json!({"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"t1","content":[
                {"type":"text","text":"a"},{"type":"image"},{"type":"text","text":"b"}]}]}})]);
        let n = normalize_transcript("claude-code", &t, DEFAULT_TRUNCATE).unwrap();
        match &n.records[1] {
            Record::Tool(tr) => assert_eq!(tr.content, "a\n[image]\nb"),
            other => panic!("expected tool, got {other:?}"),
        }
    }

    #[test]
    fn drops_bookkeeping_lines() {
        let t = jsonl(&[
            serde_json::json!({"type":"queue-operation","x":1}),
            serde_json::json!({"type":"system","x":1}),
            user("hi"),
        ]);
        let n = normalize_transcript("claude-code", &t, DEFAULT_TRUNCATE).unwrap();
        assert_eq!(n.records.len(), 2, "meta + the one real user turn");
    }

    #[test]
    fn bad_json_line_is_a_diagnostic_not_a_crash() {
        let t = format!("{{not json\n{}", jsonl(&[user("hi")]));
        let n = normalize_transcript("claude-code", &t, DEFAULT_TRUNCATE).unwrap();
        assert_eq!(n.diagnostics, vec![Diagnostic { code: "bad_json", line: 1 }]);
        assert_eq!(n.records.len(), 2);
    }

    #[test]
    fn records_split_on_newline_and_nothing_else() {
        // Every one of these is a line break to Python's splitlines() and to nothing
        // that actually writes JSONL.
        for sep in ['\u{2028}', '\u{2029}', '\u{85}', '\u{b}', '\u{c}', '\u{1c}', '\u{1d}',
                    '\u{1e}'] {
            assert_eq!(
                split_records(&format!("a{sep}b")).collect::<Vec<_>>(),
                vec![format!("a{sep}b")],
                "U+{:04X} is not a record separator",
                sep as u32
            );
        }
        assert_eq!(split_records("a\nb").collect::<Vec<_>>(), vec!["a", "b"]);
    }

    #[test]
    fn a_record_containing_u2028_survives() {
        // The regression this commit exists for. Before the fix this line was cut in
        // half, produced two bad_json diagnostics, and the record was dropped.
        let line = serde_json::json!({"type":"user","message":{"content":"a\u{2028}b"},
                                      "sessionId":"s1"})
            .to_string();
        let n = normalize_transcript("claude-code", &line, DEFAULT_TRUNCATE).unwrap();
        assert!(n.diagnostics.is_empty(), "the line is valid JSON and must parse");
        match &n.records[1] {
            Record::Text(t) => assert_eq!(t.content, "a\u{2028}b", "content survives intact"),
            other => panic!("expected the user record, got {other:?}"),
        }
    }

    #[test]
    fn a_crlf_transcript_still_parses() {
        let line = serde_json::json!({"type":"user","message":{"content":"hi"}}).to_string();
        let n = normalize_transcript("claude-code", &format!("{line}\r\n{line}"),
                                     DEFAULT_TRUNCATE)
            .unwrap();
        assert!(n.diagnostics.is_empty(), "the trailing \\r is trimmed, not parsed");
        assert_eq!(n.records.len(), 3, "meta + two user turns");
    }

    #[test]
    fn empty_transcript_has_no_meta_record() {
        let n = normalize_transcript("claude-code", "", DEFAULT_TRUNCATE).unwrap();
        assert!(n.records.is_empty(), "no records means nothing to describe");
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        // Six 3-byte characters. A byte-based cap would report the wrong remainder and
        // could split a character mid-sequence.
        let s = "日本語日本語";
        assert_eq!(truncate(s, 3), "日本語\n…[truncated 3 chars]");
        assert_eq!(truncate(s, 0), s, "0 disables truncation");
        assert_eq!(truncate(s, 99), s);
    }

    #[test]
    fn estimate_tokens_counts_characters_not_bytes() {
        assert_eq!(estimate_tokens("日本語日"), 1, "4 chars / 4 == 1, not 12 bytes / 4");
    }

    #[test]
    fn rendering_distinguishes_a_failed_tool_from_a_successful_one() {
        let recs = vec![
            Record::Tool(Tool { role: "tool", tool_call_id: None, content: "ok".into(),
                                timestamp: None, is_error: None }),
            Record::Tool(Tool { role: "tool", tool_call_id: None, content: "bad".into(),
                                timestamp: None, is_error: Some(true) }),
        ];
        assert_eq!(render_text(&recs, DEFAULT_TRUNCATE), "tool: ok\ntool ERROR: bad");
    }

    #[test]
    fn a_string_tool_input_that_is_already_json_is_kept_verbatim() {
        assert_eq!(args_str(Some(&serde_json::json!("{\"a\": 1}"))), "{\"a\": 1}");
        assert_eq!(args_str(Some(&serde_json::json!("ls -la"))), "\"ls -la\"");
        assert_eq!(args_str(None), "{}");
    }
}
