//! Finding local Claude Code sessions on disk.

use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Item {
    pub id: String,
    pub path: String,
    pub project: String,
    pub updated_at: Option<String>,
    pub size_bytes: u64,
    pub num_lines: usize,
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Listing {
    pub items: Vec<Item>,
    pub next_cursor: Option<String>,
}

pub fn default_root() -> PathBuf {
    if let Ok(env) = std::env::var("CLAUDE_PROJECTS_DIR") {
        if !env.is_empty() {
            return PathBuf::from(env);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".claude").join("projects")
}

/// UTC seconds -> `%Y-%m-%dT%H:%M:%SZ`.
///
/// civil_from_days (Howard Hinnant). This is the sixth copy in the workspace; it lives
/// here rather than in a shared crate because paos-trajectory deliberately depends on
/// nothing but serde — pulling in paos-store for a date format would undo that.
fn iso(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y, m, d, tod / 3600, (tod % 3600) / 60, tod % 60
    )
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The machine's current UTC offset in seconds.
///
/// Python's `datetime.fromisoformat(...).timestamp()` interprets a NAIVE timestamp as
/// LOCAL time. Parsing it as UTC instead would shift every ISO `--since` by the offset —
/// seven hours here — which silently widens or narrows the window rather than failing.
fn local_utc_offset_secs() -> i64 {
    let out = match std::process::Command::new("date").arg("+%z").output() {
        Ok(o) => o,
        Err(_) => return 0,
    };
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // +HHMM / -HHMM
    if s.len() < 5 {
        return 0;
    }
    let sign = if s.starts_with('-') { -1 } else { 1 };
    let hh: i64 = s[1..3].parse().unwrap_or(0);
    let mm: i64 = s[3..5].parse().unwrap_or(0);
    sign * (hh * 3600 + mm * 60)
}

/// Parse `30m` / `2h` / `7d` / `1w` or an ISO date into an epoch cutoff.
pub fn parse_since(s: Option<&str>, now: f64) -> Option<f64> {
    let s = s?.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    let (head, unit) = s.split_at(s.len() - 1);
    let mult = match unit {
        "s" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        "d" => 86_400.0,
        "w" => 604_800.0,
        _ => 0.0,
    };
    // Python guards with `len(s) >= 2 and s[:-1].replace('.','',1).isdigit()`, i.e. a
    // single optional decimal point and DIGITS ONLY — no sign, no exponent.
    if mult > 0.0 && s.chars().count() >= 2 && is_plain_decimal(head) {
        if let Ok(n) = head.parse::<f64>() {
            return Some(now - n * mult);
        }
    }
    parse_iso(&s)
}

fn is_plain_decimal(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut seen_dot = false;
    let mut digits = 0;
    for c in s.chars() {
        if c == '.' && !seen_dot {
            seen_dot = true;
        } else if c.is_ascii_digit() {
            digits += 1;
        } else {
            return false;
        }
    }
    digits > 0
}

/// `YYYY-MM-DD` with an optional `THH:MM[:SS]`, treated as local time like Python does.
fn parse_iso(s: &str) -> Option<f64> {
    let s = s.replace('z', "+00:00");
    let (date, rest) = s.split_once(['t', ' ']).unwrap_or((s.as_str(), ""));
    let p: Vec<&str> = date.split('-').collect();
    if p.len() != 3 {
        return None;
    }
    let y: i64 = p[0].parse().ok()?;
    let m: i64 = p[1].parse().ok()?;
    let d: i64 = p[2].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let mut secs = days_from_civil(y, m, d) * 86_400;

    let (time, explicit_offset) = if let Some(i) = rest.find(['+']) {
        (&rest[..i], Some(&rest[i..]))
    } else {
        (rest, None)
    };
    if !time.is_empty() {
        let t: Vec<&str> = time.split(':').collect();
        let hh: i64 = t.first()?.parse().ok()?;
        let mm: i64 = t.get(1).and_then(|v| v.parse().ok()).unwrap_or(0);
        let ss: i64 = t
            .get(2)
            .and_then(|v| v.split('.').next())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        secs += hh * 3600 + mm * 60 + ss;
    }
    // An explicit offset means the value is already absolute; a naive one is local.
    if explicit_offset.is_none() {
        secs -= local_utc_offset_secs();
    }
    Some(secs as f64)
}

/// First user prompt (snippet) + non-empty line count, in a single read.
fn title_and_lines(path: &Path) -> (Option<String>, usize) {
    use std::io::BufRead;
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return (None, 0),
    };
    let mut title: Option<String> = None;
    let mut n = 0usize;
    for line in std::io::BufReader::new(file).lines() {
        // `errors="replace"` on the Python side: a bad byte must not end the scan.
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        n += 1;
        if title.is_none() {
            if let Ok(o) = serde_json::from_str::<serde_json::Value>(&line) {
                if o.get("type").and_then(|v| v.as_str()) == Some("user") {
                    if let Some(c) =
                        o.get("message").and_then(|m| m.get("content")).and_then(|v| v.as_str())
                    {
                        let s = c.trim();
                        // Skip harness boilerplate (workspace-manager and system wrappers,
                        // slash-command envelopes) — we want the human's prompt.
                        if !s.is_empty() && !s.starts_with('<') {
                            let collapsed =
                                s.split_whitespace().collect::<Vec<_>>().join(" ");
                            title = Some(collapsed.chars().take(80).collect());
                        }
                    }
                }
            }
        }
    }
    (title, n)
}

fn item(path: &Path, mtime: i64, size: u64) -> Item {
    let (title, num_lines) = title_and_lines(path);
    Item {
        id: path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        path: path.to_string_lossy().into_owned(),
        project: path
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        updated_at: Some(iso(mtime)),
        size_bytes: size,
        num_lines,
        title,
    }
}

/// Enumerate local sessions for a source, newest-first, cursor-paginated.
pub fn list_trajectories(
    source: &str,
    limit: usize,
    cursor: Option<&str>,
    since: Option<&str>,
    root: Option<&Path>,
    now: f64,
) -> Result<Listing, String> {
    if source != "claude-code" {
        return Err(format!("unsupported source '{source}'"));
    }
    let root = root.map(PathBuf::from).unwrap_or_else(default_root);
    if !root.exists() {
        return Ok(Listing { items: Vec::new(), next_cursor: None });
    }

    // Python globbed `*/*.jsonl` — exactly one directory level, no deeper.
    //
    // mtime is kept as a FLOAT, not whole seconds. Python sorts on `st_mtime`, which
    // carries sub-second precision; truncating to seconds collapses every session
    // touched in the same second into a tie and hands the order to the tiebreak
    // instead. Caught by diffing a snapshot whose files were all copied at once.
    let mut files: Vec<(f64, u64, PathBuf)> = Vec::new();
    let dirs = match std::fs::read_dir(&root) {
        Ok(d) => d,
        Err(_) => return Ok(Listing { items: Vec::new(), next_cursor: None }),
    };
    for dir in dirs.flatten() {
        if !dir.path().is_dir() {
            continue;
        }
        let entries = match std::fs::read_dir(dir.path()) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for f in entries.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let md = match f.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            files.push((mtime, md.len(), p));
        }
    }

    if let Some(cutoff) = parse_since(since, now) {
        files.retain(|f| f.0 >= cutoff);
    }
    // Newest first, path as the tiebreak so the order is stable across runs.
    //
    // The tiebreak compares the path as a STRING, which is what Python's
    // `key=lambda f: (-f[0], str(f[2]))` does. `PathBuf`'s own Ord compares
    // component-by-component, so `b-c/x` and `b/y` come out in the opposite order
    // ('-' sorts before '/' as bytes, but "b-c" sorts after "b" as a component).
    files.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.2.to_string_lossy().cmp(&b.2.to_string_lossy()))
    });

    let start: usize = cursor.and_then(|c| c.parse().ok()).unwrap_or(0);
    let page: Vec<Item> = files
        .iter()
        .skip(start)
        .take(limit)
        .map(|(mtime, size, p)| item(p, *mtime as i64, *size))
        .collect();
    let next = if start + limit < files.len() { Some((start + limit).to_string()) } else { None };
    Ok(Listing { items: page, next_cursor: next })
}

/// Resolve a session id or path to an on-disk transcript path.
pub fn resolve_session(reference: &str, root: Option<&Path>) -> Result<String, String> {
    let p = Path::new(reference);
    if p.is_file() {
        return Ok(p.to_string_lossy().into_owned());
    }
    let root = root.map(PathBuf::from).unwrap_or_else(default_root);
    for with_ext in [true, false] {
        let mut matches: Vec<PathBuf> = Vec::new();
        if let Ok(dirs) = std::fs::read_dir(&root) {
            for dir in dirs.flatten() {
                if !dir.path().is_dir() {
                    continue;
                }
                let name =
                    if with_ext { format!("{reference}.jsonl") } else { reference.to_string() };
                let cand = dir.path().join(&name);
                if cand.is_file() {
                    matches.push(cand);
                }
            }
        }
        matches.sort();
        if let Some(first) = matches.first() {
            return Ok(first.to_string_lossy().into_owned());
        }
    }
    Err(format!("no session matching '{reference}' under {}", root.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_units() {
        let now = 1_000_000.0;
        assert_eq!(parse_since(Some("30s"), now), Some(now - 30.0));
        assert_eq!(parse_since(Some("30m"), now), Some(now - 1_800.0));
        assert_eq!(parse_since(Some("2h"), now), Some(now - 7_200.0));
        assert_eq!(parse_since(Some("7d"), now), Some(now - 604_800.0));
        assert_eq!(parse_since(Some("1w"), now), Some(now - 604_800.0));
        assert_eq!(parse_since(Some("1.5h"), now), Some(now - 5_400.0));
    }

    #[test]
    fn since_none_and_junk() {
        assert_eq!(parse_since(None, 0.0), None);
        assert_eq!(parse_since(Some(""), 0.0), None);
        assert_eq!(parse_since(Some("soon"), 0.0), None);
        // Python's isdigit() guard rejects a sign, so this is not a unit form and falls
        // through to the ISO parser, which also rejects it.
        assert_eq!(parse_since(Some("-5d"), 0.0), None);
    }

    #[test]
    fn since_iso_date() {
        // 2026-01-01 UTC. The local-offset correction is applied on top, so assert the
        // value lands on the right day rather than on an exact second.
        let v = parse_since(Some("2026-01-01"), 0.0).expect("an ISO date parses");
        let day = (v as i64).div_euclid(86_400);
        assert!(
            (days_from_civil(2026, 1, 1) - day).abs() <= 1,
            "expected 2026-01-01 +/- one day of timezone offset, got {}",
            iso(v as i64)
        );
    }

    #[test]
    fn iso_formats_epoch_as_utc() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(1_767_225_600), "2026-01-01T00:00:00Z");
    }

    #[test]
    fn civil_round_trips() {
        for (y, m, d) in [(1970, 1, 1), (2000, 2, 29), (2026, 7, 31), (1999, 12, 31)] {
            let secs = days_from_civil(y, m, d) * 86_400;
            assert_eq!(iso(secs), format!("{y:04}-{m:02}-{d:02}T00:00:00Z"));
        }
    }

    #[test]
    fn missing_root_is_empty_not_an_error() {
        let l = list_trajectories(
            "claude-code",
            10,
            None,
            None,
            Some(Path::new("/nope/does/not/exist")),
            0.0,
        )
        .unwrap();
        assert!(l.items.is_empty());
        assert_eq!(l.next_cursor, None);
    }

    #[test]
    fn unsupported_source_is_rejected() {
        assert!(list_trajectories("cursor", 10, None, None, None, 0.0).is_err());
    }
}
