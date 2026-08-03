//! Python-compatible JSON emission.
//!
//! This exists because `serde_json::to_string` and `json.dumps` disagree in three ways
//! that all show up in real transcripts, and each one makes an otherwise-correct port
//! fail a byte-for-byte diff:
//!
//! 1. **Separators.** `json.dumps(x)` defaults to `", "` and `": "` — WITH spaces.
//!    serde_json emits `,` and `:`. Every tool call in every transcript differs.
//! 2. **Non-ASCII.** `json.dumps(x, indent=2)` defaults to `ensure_ascii=True`, so `é`
//!    becomes `é` and an emoji becomes a surrogate PAIR. serde_json passes both
//!    through as UTF-8.
//! 3. **Key order.** Handled by the `preserve_order` feature on serde_json (see the
//!    workspace manifest), not here.
//!
//! Verified against CPython's `json` module — see the tests at the bottom, whose
//! expectations were produced by running `json.dumps` rather than written from memory.

use serde_json::Value;

/// `json.dumps(v, ensure_ascii=False)` — compact, non-ASCII left intact.
pub fn dumps_compact(v: &Value) -> String {
    let mut out = String::new();
    write_compact(v, &mut out);
    out
}

/// `json.dumps(v, indent=2)` — pretty, non-ASCII escaped.
pub fn dumps_pretty(v: &Value) -> String {
    let mut out = String::new();
    write_pretty(v, 0, &mut out);
    out
}

fn write_compact(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&number(n)),
        Value::String(s) => escape(s, false, out),
        Value::Array(a) => {
            out.push('[');
            for (i, item) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_compact(item, out);
            }
            out.push(']');
        }
        Value::Object(o) => {
            out.push('{');
            for (i, (k, val)) in o.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                escape(k, false, out);
                out.push_str(": ");
                write_compact(val, out);
            }
            out.push('}');
        }
    }
}

fn write_pretty(v: &Value, depth: usize, out: &mut String) {
    match v {
        Value::Array(a) if !a.is_empty() => {
            out.push_str("[\n");
            for (i, item) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                indent(depth + 1, out);
                write_pretty(item, depth + 1, out);
            }
            out.push('\n');
            indent(depth, out);
            out.push(']');
        }
        Value::Object(o) if !o.is_empty() => {
            out.push_str("{\n");
            for (i, (k, val)) in o.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                indent(depth + 1, out);
                escape(k, true, out);
                out.push_str(": ");
                write_pretty(val, depth + 1, out);
            }
            out.push('\n');
            indent(depth, out);
            out.push('}');
        }
        // Empty containers stay on one line — `json.dumps({"a": []}, indent=2)` gives
        // `"a": []`, not `"a": [\n]`.
        Value::Array(_) => out.push_str("[]"),
        Value::Object(_) => out.push_str("{}"),
        Value::String(s) => escape(s, true, out),
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&number(n)),
    }
}

fn indent(depth: usize, out: &mut String) {
    for _ in 0..depth * 2 {
        out.push(' ');
    }
}

/// Python renders a float that is integral as `1.0`; serde_json's `Number` display
/// already matches for the shapes that appear in transcripts, but a whole-valued f64
/// prints as `1` without this.
fn number(n: &serde_json::Number) -> String {
    if n.is_f64() {
        if let Some(f) = n.as_f64() {
            if f.fract() == 0.0 && f.is_finite() && f.abs() < 1e16 {
                return format!("{f:.1}");
            }
        }
    }
    n.to_string()
}

/// Escape a string the way CPython's json encoder does.
///
/// `ascii_only` mirrors `ensure_ascii`: anything outside the printable ASCII range
/// becomes `\uXXXX`, and a codepoint above the BMP becomes a UTF-16 SURROGATE PAIR
/// (`😀` -> `😀`), which is what Python emits and what a naive single
/// `\u{1F600}` escape would get wrong.
fn escape(s: &str, ascii_only: bool, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if ascii_only && (c as u32) > 0x7e => {
                let cp = c as u32;
                if cp > 0xFFFF {
                    let v = cp - 0x10000;
                    out.push_str(&format!("\\u{:04x}", 0xD800 + (v >> 10)));
                    out.push_str(&format!("\\u{:04x}", 0xDC00 + (v & 0x3FF)));
                } else {
                    out.push_str(&format!("\\u{cp:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every expectation below is the literal output of CPython's json.dumps, captured by
    // running it — not reconstructed from the docs.

    #[test]
    fn compact_uses_pythons_spaced_separators() {
        let v: Value = serde_json::from_str(r#"{"command":"ls","a":1}"#).unwrap();
        assert_eq!(dumps_compact(&v), r#"{"command": "ls", "a": 1}"#);
        let a: Value = serde_json::from_str("[1,2]").unwrap();
        assert_eq!(dumps_compact(&a), "[1, 2]");
    }

    #[test]
    fn preserve_order_keeps_transcript_key_order() {
        // The exact shape that made this necessary: alphabetical sorting would put
        // new_string before old_string and file_path first.
        let v: Value =
            serde_json::from_str(r#"{"file_path":"/a","old_string":"x","new_string":"y"}"#)
                .unwrap();
        assert_eq!(
            dumps_compact(&v),
            r#"{"file_path": "/a", "old_string": "x", "new_string": "y"}"#,
            "serde_json must be built with the preserve_order feature"
        );
    }

    #[test]
    fn compact_leaves_non_ascii_alone() {
        let v: Value = serde_json::from_str(r#"{"c":"é·⏵"}"#).unwrap();
        assert_eq!(dumps_compact(&v), "{\"c\": \"é·⏵\"}");
    }

    #[test]
    fn pretty_matches_python_indent_two() {
        let v: Value =
            serde_json::from_str(r#"{"a":[],"b":{},"c":"é·⏵","d":null,"e":1.0}"#).unwrap();
        assert_eq!(
            dumps_pretty(&v),
            "{\n  \"a\": [],\n  \"b\": {},\n  \"c\": \"\\u00e9\\u00b7\\u23f5\",\n  \"d\": null,\n  \"e\": 1.0\n}"
        );
    }

    #[test]
    fn pretty_escapes_astral_chars_as_surrogate_pairs() {
        let v: Value = serde_json::from_str(r#"{"a":"😀"}"#).unwrap();
        assert_eq!(dumps_pretty(&v), "{\n  \"a\": \"\\ud83d\\ude00\"\n}");
    }

    #[test]
    fn pretty_escapes_control_characters() {
        let v: Value = serde_json::from_str(r#"{"a":"line\nbreak\ttab\"q\"\\ back"}"#).unwrap();
        assert_eq!(
            dumps_pretty(&v),
            "{\n  \"a\": \"line\\nbreak\\ttab\\\"q\\\"\\\\ back\"\n}"
        );
    }

    #[test]
    fn pretty_nests() {
        let v: Value = serde_json::from_str(r#"{"n":[1,{"k":"v"}]}"#).unwrap();
        assert_eq!(
            dumps_pretty(&v),
            "{\n  \"n\": [\n    1,\n    {\n      \"k\": \"v\"\n    }\n  ]\n}"
        );
    }

    #[test]
    fn empty_top_level_containers() {
        assert_eq!(dumps_pretty(&serde_json::json!({})), "{}");
        assert_eq!(dumps_pretty(&serde_json::json!([])), "[]");
        assert_eq!(dumps_compact(&serde_json::json!({})), "{}");
        assert_eq!(dumps_compact(&serde_json::json!([])), "[]");
    }
}
