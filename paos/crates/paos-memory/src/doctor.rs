//! `paos doctor` — does this system still do what it claims?
//!
//! Written after three failures in one day that shared a single shape: a capability
//! stopped working, nothing errored, and the documentation kept promising it.
//!
//!   * `approve()` wrote to cognee while recall read paos.db. Five of six approved
//!     memories never became recallable. No error, ever.
//!   * The nightly dream lived in the Python daemon; the Rust cutover retired that
//!     process and the schedule went with it. The policy file still said "on by default".
//!   * Sessions went deaf on Telegram while showing as online.
//!
//! Unit tests cannot catch this class: each component was individually correct and
//! well-tested. What was missing was anyone asking whether the wiring between them
//! still existed. So this checks END STATES with evidence — "a memory was written 3h
//! ago", "the last dream was 5 days ago" — rather than asking components if they feel
//! healthy.
//!
//! Every check reports what it OBSERVED, not just a verdict, because a red light with
//! no evidence is one more thing to distrust.

use rusqlite::Connection;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

impl Level {
    fn mark(self) -> &'static str {
        match self {
            Level::Ok => "✓",
            Level::Warn => "!",
            Level::Fail => "✗",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub level: Level,
    /// What was actually observed. A verdict without evidence is not actionable.
    pub detail: String,
    /// What to do about it. Omitted when there is nothing to do.
    pub fix: Option<String>,
}

impl Check {
    /// Is a session BLOCKED right now, or can this wait until morning?
    ///
    /// The operator's attention is the scarcest resource on this machine, and every
    /// subsystem competes for it through one channel. Without this distinction a stale
    /// nightly job buzzes the phone as loudly as "sessions cannot remember anything",
    /// and the predictable result is that both get muted.
    ///
    /// Derived from the check rather than stored on it: urgency is a property of WHICH
    /// subsystem failed, and keeping it in one place stops a new check from silently
    /// defaulting into the loud category.
    pub fn is_urgent(&self) -> bool {
        if self.level != Level::Fail {
            return false;
        }
        match self.name {
            // Being unreachable, or sessions unable to remember, stops work now.
            // A stuck spool means the fleet believes it is remembering and is not.
            "telegram" | "memory writes" | "memory spool" => true,
            // A thrashing or full machine degrades every session at once.
            // E2BIG takes the Bash tool offline entirely — nothing is more urgent.
            "swap" | "disk" | "runaway build" | "worktrees" => true,
            // A missing backup is the only UNRECOVERABLE failure here, but it does not
            // stop work in the next hour — it is delivered silently, not as an alarm.
            // A missed nightly pass or an unread queue costs nothing before morning.
            _ => false,
        }
    }
}

/// A memory store nobody has written to in this long suggests the write path is broken
/// rather than that the operator was quiet — sessions remember things constantly.
///
/// 48, not 72, and the number is measured rather than chosen. Over 38 days of real
/// history the LONGEST gap between consecutive writes was 39.1 h, the next 37.3 h, and 37
/// of 38 days had at least one write. So 72 h was nearly double the natural maximum: a
/// silently broken write path had three days of cover, on the one capability whose failure
/// this check exists to catch. 48 h keeps ~9 h of margin over the worst real quiet period
/// while halving the blind spot.
const STALE_WRITE_HOURS: i64 = 48;
/// Proposals sitting unreviewed this long mean the queue is being ignored, which is the
/// state that made `curate`'s output worthless.
const STALE_QUEUE_DAYS: i64 = 14;
/// A dream older than this, when enabled, means the schedule is not running.
const STALE_DREAM_DAYS: i64 = 3;
/// Telegram long-polls on a ~50s cycle, so a few minutes of silence is already abnormal.
/// Generous enough to survive one restart, tight enough to notice within a coffee break.
const TELEGRAM_SILENT_MINS: i64 = 5;

pub fn run(conn: &Connection) -> Vec<Check> {
    vec![
        memory_writes(conn),
        telegram(conn),
        dream(conn),
        review_queue(conn),
        spool(),
        swap(),
        runaway(),
        worktrees(),
        disk(),
        backups(conn),
        cognee(),
    ]
}

/// Is the Telegram bridge still talking to Telegram?
///
/// It long-polls continuously, so a gap of minutes means it is wedged or the daemon is
/// not running the bridge at all. Worth its own check because the failure is invisible
/// from the operator's side: messages simply stop being answered, which looks exactly
/// like a session that is busy. That happened, and the diagnosis took far too long.
fn telegram(conn: &Connection) -> Check {
    let last: i64 = conn
        .query_row("SELECT value FROM meta WHERE key='telegram.last_poll_epoch'", [], |r| {
            r.get::<_, String>(0)
        })
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    if last == 0 {
        return Check {
            name: "telegram",
            level: Level::Warn,
            detail: "no poll recorded — bridge not configured, or not started yet".into(),
            fix: Some("check ~/.claude/skills/paos/.env and `launchctl print gui/$(id -u)/ai.paos.daemon`".into()),
        };
    }
    let mins = (now_epoch() - last) / 60;
    if mins > TELEGRAM_SILENT_MINS {
        Check {
            name: "telegram",
            level: Level::Fail,
            detail: format!("bridge has not reached Telegram for {mins}m"),
            fix: Some("your phone is disconnected from the fleet — restart paosd".into()),
        }
    } else {
        Check {
            name: "telegram",
            level: Level::Ok,
            detail: if mins < 1 { "bridge live".into() } else { format!("bridge live, last poll {mins}m ago") },
            fix: None,
        }
    }
}

pub fn render(checks: &[Check]) -> Vec<String> {
    let mut out = Vec::new();
    for c in checks {
        out.push(format!("{} {:<14} {}", c.level.mark(), c.name, c.detail));
        if let Some(f) = &c.fix {
            out.push(format!("                 → {f}"));
        }
    }
    let bad = checks.iter().filter(|c| c.level != Level::Ok).count();
    out.push(String::new());
    out.push(if bad == 0 {
        "all clear".to_string()
    } else {
        format!("{bad} of {} checks need attention", checks.len())
    });
    out
}

/// Is anything still landing in the store?
///
/// This is the check that would have caught the approve-path bug on day one: writes were
/// "succeeding" into a retired system, and the only observable difference was that
/// nothing new appeared here.
fn memory_writes(conn: &Connection) -> Check {
    let newest: Option<String> = conn
        .query_row(
            "SELECT max(created_ts) FROM memories WHERE superseded IS NULL",
            [],
            |r| r.get(0),
        )
        .ok()
        .flatten();
    let total: i64 = conn
        .query_row("SELECT count(*) FROM memories WHERE superseded IS NULL", [], |r| r.get(0))
        .unwrap_or(0);
    match newest {
        None => Check {
            name: "memory writes",
            level: Level::Fail,
            detail: "the store is empty".into(),
            fix: Some("paos remember <fact> --project, then check it comes back".into()),
        },
        Some(ts) => {
            let hours = hours_since_iso(&ts);
            let detail = format!("{total} facts, newest {}", ago(hours));
            if hours > STALE_WRITE_HOURS {
                Check {
                    name: "memory writes",
                    level: Level::Warn,
                    detail,
                    // Deliberately phrased as a test, not a reassurance: the last time
                    // this looked wrong, the write path was silently writing elsewhere.
                    fix: Some(
                        "nothing stored in days — verify with: paos remember 'doctor probe' \
                         --project && paos recall 'doctor probe'"
                            .into(),
                    ),
                }
            } else {
                Check { name: "memory writes", level: Level::Ok, detail, fix: None }
            }
        }
    }
}

/// Is the nightly pass both switched on and actually running?
///
/// Split deliberately: "off" is a decision and reports as OK, while "on but not running"
/// is the silent failure. Conflating them is how the schedule stayed dead for days while
/// the config still said enabled.
fn dream(conn: &Connection) -> Check {
    let enabled = conn
        .query_row(
            "SELECT value FROM paos_config WHERE key='dream_enabled'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .map(|v| is_truthy(&v))
        .unwrap_or(true);
    let last: i64 = conn
        .query_row(
            "SELECT value FROM meta WHERE key='dream.last_run_epoch'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);

    if !enabled {
        return Check {
            name: "nightly dream",
            level: Level::Ok,
            detail: "disabled by config (dream_enabled=0)".into(),
            fix: None,
        };
    }
    if last == 0 {
        return Check {
            name: "nightly dream",
            level: Level::Warn,
            detail: "enabled, but has never run under this daemon".into(),
            fix: Some("normal right after an upgrade; if it persists past 06:00, check paosd logs".into()),
        };
    }
    // Started recently but never recorded a finish: the run is being killed partway,
    // which a "last ran 0d ago" reading would happily call healthy.
    let last_ok: i64 = conn
        .query_row("SELECT value FROM meta WHERE key='dream.last_ok_epoch'", [], |r| {
            r.get::<_, String>(0)
        })
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    if last_ok < last - 3600 {
        return Check {
            name: "nightly dream",
            level: Level::Warn,
            detail: "last run started but never finished".into(),
            fix: Some("a restart during the 03:00-06:00 window kills it; check paosd logs".into()),
        };
    }
    let days = (now_epoch() - last) / 86_400;
    if days > STALE_DREAM_DAYS {
        Check {
            name: "nightly dream",
            level: Level::Fail,
            detail: format!("enabled but last ran {days}d ago"),
            fix: Some("the schedule is not firing — check paosd is running and the 03:00-06:00 window".into()),
        }
    } else {
        Check {
            name: "nightly dream",
            level: Level::Ok,
            detail: format!("enabled, last ran {days}d ago"),
            fix: None,
        }
    }
}

/// Swap pressure.
///
/// Measured while writing this: 8393 MB of 9216 MB in use — 91%. Nothing on this machine
/// would ever have said so. `dash-resources` parses the same sysctl and then only PRINTS
/// it; the sole threshold-and-act loop anywhere is the Claude quota auto-switch. On a
/// 24 GB box running many parallel agents this is the failure that makes everything slow
/// at once while every dashboard still reports green.
///
/// 85% not 95%: past ~90% the machine is already thrashing and the operator has been
/// suffering for a while. The point is to speak before that, not to confirm it.
/// Stale git worktree registrations, which can take the Bash tool offline entirely.
///
/// The Claude Code sandbox profile embeds ONE DENY PATH PER REGISTERED WORKTREE. Enough of
/// them and the profile plus environment exceeds the OS exec argument limit, so every
/// spawn fails with E2BIG — not one command, EVERY command. Seen on this machine on
/// 2026-07-31: 228 registered in one repo, 114 of them stale.
///
/// A session in that state cannot diagnose itself, because diagnosing requires running a
/// command and no command can run. That is the same reason the runaway check lives here:
/// paosd is outside the sandbox and can look when nothing else can.
///
/// Counts only STALE entries — the worktree directory is gone but git still registers it.
/// Those are pure waste with a safe one-line fix. Live worktrees are someone's work and
/// are not this check's business.
fn stale_worktrees() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let mut total = 0usize;
    let mut worst: Option<(String, usize)> = None;
    // The roots this machine keeps repos in. Missing ones are skipped, so this is a
    // no-op on a machine laid out differently rather than an error.
    for root in ["Dev", "Projects", "conductor/workspaces"] {
        let base = std::path::Path::new(&home).join(root);
        let Ok(entries) = std::fs::read_dir(&base) else { continue };
        for e in entries.flatten() {
            for candidate in [e.path(), e.path().join(e.file_name())] {
                let wt = candidate.join(".git").join("worktrees");
                let Ok(regs) = std::fs::read_dir(&wt) else { continue };
                let mut n = 0usize;
                for r in regs.flatten() {
                    // `gitdir` points at <worktree>/.git; the worktree is its parent.
                    let Ok(g) = std::fs::read_to_string(r.path().join("gitdir")) else { continue };
                    let dir = g.trim().trim_end_matches("/.git");
                    if !dir.is_empty() && !std::path::Path::new(dir).exists() {
                        n += 1;
                    }
                }
                if n > 0 {
                    total += n;
                    let name = candidate.file_name()
                        .map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
                    if worst.as_ref().is_none_or(|(_, w)| n > *w) {
                        worst = Some((name, n));
                    }
                }
            }
        }
    }
    if total < STALE_WORKTREE_LIMIT {
        return None;
    }
    let (repo, n) = worst.unwrap_or_default();
    Some(format!(
        "{total} stale worktree registrations ({n} in {repo}). Each one adds a deny path to          the sandbox profile; enough of them and EVERY Bash command fails with E2BIG"))
}

/// Well below the ~228 that broke a session, and well above normal churn (this machine
/// carries about 15 live worktrees in total).
const STALE_WORKTREE_LIMIT: usize = 40;

/// Stale worktree registrations, which a session cannot see and cannot survive.
fn worktrees() -> Check {
    match stale_worktrees() {
        Some(detail) => Check {
            name: "worktrees",
            level: Level::Fail,
            detail,
            fix: Some("git -C <repo> worktree prune   (safe: only removes entries whose \
                       directory is already gone)".into()),
        },
        None => Check {
            name: "worktrees",
            level: Level::Ok,
            detail: "no meaningful stale registrations".into(),
            fix: None,
        },
    }
}

/// An orphaned build or test process, which no session can see for itself.
fn runaway() -> Check {
    match runaway_build() {
        Some(detail) => Check {
            name: "runaway build",
            level: Level::Fail,
            detail,
            fix: Some("kill it — a session cannot: ps/pgrep/kill are denied in the sandbox".into()),
        },
        None => Check {
            name: "runaway build",
            level: Level::Ok,
            detail: "no long-running build or test process".into(),
            fix: None,
        },
    }
}

fn swap() -> Check {
    let Some((used, total)) = read_swap() else {
        return Check { name: "swap", level: Level::Ok, detail: "unreadable".into(), fix: None };
    };
    if total <= 0.0 {
        return Check { name: "swap", level: Level::Ok, detail: "none in use".into(), fix: None };
    }
    let pct = 100.0 * used / total;
    let detail = format!("{:.0}% used ({:.1} of {:.1} GB)", pct, used / 1024.0, total / 1024.0);
    if pct >= 85.0 {
        Check {
            name: "swap",
            level: Level::Fail,
            detail,
            fix: Some(top_memory_hog().unwrap_or_else(
                || "check what is resident: ps -A -o rss,comm | sort -rn | head".into())),
        }
    } else {
        Check { name: "swap", level: Level::Ok, detail, fix: None }
    }
}

/// Free space on the volume `$HOME` lives on.
///
/// 20 GB rather than a percentage: what matters is whether a build, a model download or
/// a database write can still complete, and that is an absolute quantity.
fn disk() -> Check {
    let Some(free_gb) = read_disk_free_gb() else {
        return Check { name: "disk", level: Level::Ok, detail: "unreadable".into(), fix: None };
    };
    let detail = format!("{free_gb:.0} GB free");
    if free_gb < 20.0 {
        Check {
            name: "disk",
            level: Level::Fail,
            detail,
            fix: Some("much of what fills this disk is regenerable cache — \
                       ~/Library/Caches/go-build, ~/.lmstudio, brew, uv, bun".into()),
        }
    } else {
        Check { name: "disk", level: Level::Ok, detail, fix: None }
    }
}

fn read_swap() -> Option<(f64, f64)> {
    let out = std::process::Command::new("sysctl").args(["-n", "vm.swapusage"]).output().ok()?;
    parse_swap(&String::from_utf8_lossy(&out.stdout))
}

/// Split from the syscall so the PARSER can be tested deterministically.
///
/// The first version of this test asserted that THIS machine's sysctl was readable, which
/// passes in a terminal and fails inside the agent sandbox where sysctl is blocked. A test
/// whose result depends on where it runs teaches you to re-run instead of read — and the
/// parser is the part that was actually broken, twice.
///
/// Input: "total = 9216.00M  used = 8393.00M  free = 823.00M  (encrypted)"
fn parse_swap(s: &str) -> Option<(f64, f64)> {
    // Tokenise rather than slice: the first version trimmed from the END of the remainder
    // of the line, which for "used = 8393.00M  free = 823.00M (encrypted)" left the whole
    // tail and failed to parse — rendering as a permanently reassuring "unreadable".
    // Values carry a unit suffix (M, or G on a larger machine), so scale it.
    let toks: Vec<&str> = s.split_whitespace().collect();
    let mb = |key: &str| -> Option<f64> {
        let i = toks.iter().position(|t| *t == key)?;
        let raw = toks.get(i + 2)?;          // key, "=", value
        let (num, unit) = raw.split_at(raw.find(|c: char| c.is_ascii_alphabetic())?);
        let v: f64 = num.parse().ok()?;
        Some(match unit.chars().next()? {
            'G' | 'g' => v * 1024.0,
            'K' | 'k' => v / 1024.0,
            _ => v,                          // M
        })
    };
    Some((mb("used")?, mb("total")?))
}

fn read_disk_free_gb() -> Option<f64> {
    let home = std::env::var("HOME").ok()?;
    let out = std::process::Command::new("df").args(["-k", &home]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let line = s.lines().nth(1)?;
    let avail_kb: f64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(avail_kb / 1048576.0)
}

/// Name the biggest resident process, because "swap is full" without "OrbStack is holding
/// 6 GB" is a fact you cannot act on.
fn top_memory_hog() -> Option<String> {
    let out = std::process::Command::new("sh")
        // head -1, not -2: `sort -rn` sorts the "RSS" header as 0 and pushes it to the
        // END, so the first line is already the largest. Taking the second reported
        // WebKit at 1.8 GB while OrbStack sat on 6.2 GB — a true statement about the
        // wrong process, which is worse than no hint.
        .args(["-c", "ps -A -o rss,comm | sort -rn | head -1"])
        .output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let mut parts = s.trim().splitn(2, char::is_whitespace);
    let rss_kb: f64 = parts.next()?.parse().ok()?;
    let name = parts.next()?.rsplit('/').next()?.trim();
    Some(format!("largest resident process: {name} at {:.1} GB", rss_kb / 1048576.0))
}

/// A build or test process that has been burning CPU for far too long.
///
/// THE CHECK THAT WOULD HAVE ENDED A SEVEN-HOUR INCIDENT IN ONE CALL. On 2026-07-31 a
/// `cargo test` orphaned in a worktree and its child burned 432 MINUTES of CPU at ~1.7
/// cores before anyone noticed — while a peer in another repo spent the afternoon
/// measuring a "scheduler starvation" defect on the same box.
///
/// Two things make it invisible from a session:
///   * `ps`, `pgrep` and `kill` are DENIED inside the agent sandbox, and the denial exits
///     0 and reads like a clean empty result — so its owner could not see it, confirm it
///     or stop it. paosd runs OUTSIDE the sandbox, so the daemon is the only thing on the
///     machine that can look.
///   * `cargo test` forks a harness binary and then `wait()`s on it, so the PARENT reads
///     0.0% CPU for a perfectly healthy run. Two of us checked the parent and concluded
///     it was idle. It carries no information at all.
///
/// So this keys on CUMULATIVE CPU TIME, which cannot mislead in either direction: a
/// process with 432 minutes accumulated did that work whatever a single sample says, and
/// one with 0.13 seconds after seven hours has done nothing however alive it looks.
///
/// Scoped to BUILD AND TEST artefacts on purpose. A browser or a VM legitimately
/// accumulates hours, so flagging "high CPU" outright would fire constantly and be
/// learned-around; a compiler or a test binary running for half an hour is not normal.
fn runaway_build() -> Option<String> {
    let out = std::process::Command::new("sh")
        // `time` is cumulative CPU (MM:SS.ss or HH:MM:SS); `-r` sorts by current CPU.
        .args(["-c", "ps -Ao time,pid,%cpu,comm -r | head -25"])
        .output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    for line in s.lines().skip(1) {
        let mut f = line.split_whitespace();
        let (time, pid, cpu) = (f.next()?, f.next()?, f.next()?);
        // COMM is the REST of the line, not the next token: real paths contain spaces
        // (OrbStack's is ".../OrbStack Helper.app/.../OrbStack Helper"), and truncating at
        // the first space would drop the "/target/debug/" segment this check keys on —
        // failing CLOSED, i.e. missing a runaway rather than reporting a false one.
        let comm = f.collect::<Vec<_>>().join(" ");
        let comm = comm.as_str();
        if !is_build_artifact(comm) {
            continue;
        }
        let mins = cpu_minutes(time).unwrap_or(0.0);
        if mins >= RUNAWAY_CPU_MINUTES {
            let name = comm.rsplit('/').next().unwrap_or(comm);
            return Some(format!(
                "{name} (pid {pid}) has used {mins:.0} min of CPU and is at {cpu}% — a                  build/test process this old is usually orphaned. Check it:                  ps -o pid,ppid,stat,time,%cpu,etime -p {pid}"));
        }
    }
    None
}

/// Cumulative CPU minutes from `ps` TIME (`MM:SS.ss` or `HH:MM:SS`).
fn cpu_minutes(t: &str) -> Option<f64> {
    let parts: Vec<&str> = t.split(':').collect();
    match parts.len() {
        2 => Some(parts[0].parse::<f64>().ok()? + parts[1].parse::<f64>().ok()? / 60.0),
        3 => Some(parts[0].parse::<f64>().ok()? * 60.0 + parts[1].parse::<f64>().ok()?
                  + parts[2].parse::<f64>().ok()? / 60.0),
        _ => None,
    }
}

/// Is this a compiler, test harness or build tool — something that should be short-lived?
fn is_build_artifact(comm: &str) -> bool {
    let c = comm.rsplit('/').next().unwrap_or(comm);
    comm.contains("/target/debug/") || comm.contains("/target/release/")
        || comm.contains("/node_modules/")
        || matches!(c, "cargo" | "rustc" | "pytest" | "jest" | "vitest" | "tsc" | "webpack")
}

/// Half an hour of CPU. A real `cargo build --release` on this tree is ~40s of wall clock
/// and well under this; the incident that motivated it had 432 minutes.
const RUNAWAY_CPU_MINUTES: f64 = 30.0;

/// Are sessions' memory writes actually being picked up?
///
/// Agent sandboxes block the daemon's socket, so every session's `remember` lands in a
/// spool file for paosd to ingest. That mechanism is only as good as the ingestion: if
/// the thread stops, writes pile up as files and the fleet believes it is remembering
/// while nothing reaches the store. Exactly the silent-capability-loss this whole report
/// exists to catch, and I introduced it today — so it gets a check.
fn spool() -> Check {
    let dir = std::path::PathBuf::from(
        std::env::var("PAOS_ROOT").unwrap_or_else(|_| {
            format!("{}/.paos", std::env::var("HOME").unwrap_or_default())
        })).join("spool");
    spool_at(&dir)
}

/// Split from `spool()` so tests can point at a directory WITHOUT mutating PAOS_ROOT.
/// Environment variables are process-global and the test harness runs threads in
/// parallel, so env-mutating tests race each other — which is how the first version of
/// these tests failed intermittently.
fn spool_at(dir: &std::path::Path) -> Check {
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Check { name: "memory spool", level: Level::Ok,
                       detail: "empty".into(), fix: None };
    };
    let now = std::time::SystemTime::now();
    let (mut pending, mut oldest_secs, mut bad) = (0usize, 0u64, 0usize);
    for e in entries.flatten() {
        match e.path().extension().and_then(|x| x.to_str()) {
            // .bad files are writes paosd could not parse. They are KEPT rather than
            // deleted so a fact is never lost silently — but they need a human.
            Some("bad") => bad += 1,
            Some("json") => {
                pending += 1;
                if let Ok(age) = e.metadata().and_then(|m| m.modified()) {
                    if let Ok(d) = now.duration_since(age) {
                        oldest_secs = oldest_secs.max(d.as_secs());
                    }
                }
            }
            _ => {}
        }
    }
    if bad > 0 {
        return Check {
            name: "memory spool",
            level: Level::Fail,
            detail: format!("{bad} write(s) paosd could not parse (kept, not lost)"),
            // Not necessarily a FACT: the spool has carried bus, operator and (since
            // 2026-08-02) task writes for a while, so naming one of them sends the reader
            // looking in the wrong place. Each file names its own `op`.
            fix: Some("inspect ~/.paos/spool/*.bad — each is a write a session tried to make; \
                       the `op` field says which kind".into()),
        };
    }
    // The drain runs every 5 seconds, so ANY sustained backlog means it is not running.
    // This was 30 minutes when the drain rode a 10-minute tick, which let doctor report
    // green while a session's fact sat unwritten — a green doctor and a broken write path
    // coexisting is the exact thing doctor exists to prevent.
    if pending > 0 && oldest_secs > 120 {
        return Check {
            name: "memory spool",
            level: Level::Fail,
            detail: format!("{pending} write(s) stuck, oldest {}s — sessions think they remembered",
                            oldest_secs),
            fix: Some("paosd is not draining the spool; check it is running".into()),
        };
    }
    Check {
        name: "memory spool",
        level: Level::Ok,
        detail: if pending == 0 { "empty".into() } else { format!("{pending} queued, draining") },
        fix: None,
    }
}

/// Is there a recent copy of the database anywhere else?
///
/// The only finding on this machine where the failure is UNRECOVERABLE. Everything else
/// doctor reports can be fixed by restarting something; a dead SSD with no snapshot loses
/// every fact the agents ever learned.
fn backups(conn: &Connection) -> Check {
    let last: i64 = conn
        .query_row("SELECT value FROM meta WHERE key='backup.last_ok_epoch'", [], |r| {
            r.get::<_, String>(0)
        })
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    if last == 0 {
        return Check {
            name: "backups",
            level: Level::Fail,
            detail: "NO backup has ever succeeded".into(),
            fix: Some("paos backup run — takes a verified snapshot to Google Drive".into()),
        };
    }
    let days = (now_epoch() - last) / 86_400;
    if days > 2 {
        Check {
            name: "backups",
            level: Level::Warn,
            detail: format!("newest backup is {days}d old"),
            fix: Some("paos backup status — the nightly pass may be failing".into()),
        }
    } else {
        Check { name: "backups", level: Level::Ok, detail: format!("last backup {days}d ago"), fix: None }
    }
}

/// Proposals are useless if nobody looks at them, and a queue that accumulates is
/// evidence the operator stopped trusting it.
fn review_queue(conn: &Connection) -> Check {
    let (pending, oldest): (i64, Option<String>) = conn
        .query_row(
            "SELECT count(*), min(created_ts) FROM memory_proposals WHERE status='pending'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, None));
    if pending == 0 {
        return Check {
            name: "review queue",
            level: Level::Ok,
            detail: "nothing pending".into(),
            fix: None,
        };
    }
    let days = oldest.map(|t| hours_since_iso(&t) / 24).unwrap_or(0);
    let detail = format!("{pending} pending, oldest {days}d");
    if days > STALE_QUEUE_DAYS {
        Check {
            name: "review queue",
            level: Level::Warn,
            detail,
            fix: Some("paos memory review — a queue nobody empties stops being read at all".into()),
        }
    } else {
        Check { name: "review queue", level: Level::Ok, detail, fix: None }
    }
}

/// cognee was replaced by the local store. If it is still listening, it is holding
/// gigabytes for nothing — worth saying once rather than leaving it to be rediscovered.
fn cognee() -> Check {
    let up = std::net::TcpStream::connect_timeout(
        &"127.0.0.1:8000".parse().expect("literal address"),
        std::time::Duration::from_millis(250),
    )
    .is_ok();
    if up {
        Check {
            name: "cognee",
            level: Level::Warn,
            detail: "still listening on :8000, but nothing reads it since the migration".into(),
            fix: Some("reclaim it once you are sure nothing needs the old graph".into()),
        }
    } else {
        Check { name: "cognee", level: Level::Ok, detail: "retired".into(), fix: None }
    }
}

/// Shared truthiness for config flags, used by the dream scheduler and this report.
/// One definition: a switch the scheduler honours but the report misreads (or the
/// reverse) is worse than either alone.
pub fn is_truthy(v: &str) -> bool {
    !matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    )
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Hours since an ISO-ish UTC timestamp. Both "2026-07-30T12:00:00Z" and the Python's
/// "2026-07-30 12:00:00.123" appear in this database, so parse the leading fields and
/// ignore the rest rather than demanding one format.
pub fn hours_since_iso(ts: &str) -> i64 {
    let Some(epoch) = parse_epoch(ts) else {
        return 0;
    };
    (now_epoch() - epoch).max(0) / 3600
}

fn parse_epoch(ts: &str) -> Option<i64> {
    let b = ts.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |a: usize, z: usize| ts.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, s) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    // Days from civil (Howard Hinnant's algorithm) — no chrono in this workspace.
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + s)
}

fn ago(hours: i64) -> String {
    if hours < 1 {
        "just now".into()
    } else if hours < 48 {
        format!("{hours}h ago")
    } else {
        format!("{}d ago", hours / 24)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE memories(id TEXT, dataset TEXT, text TEXT, created_ts TEXT, superseded TEXT);
             CREATE TABLE memory_proposals(id INTEGER PRIMARY KEY, status TEXT, created_ts TEXT);
             CREATE TABLE paos_config(key TEXT PRIMARY KEY, value TEXT);
             CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        c
    }

    fn iso(epoch: i64) -> String {
        let days = epoch.div_euclid(86_400);
        let tod = epoch.rem_euclid(86_400);
        let (mut y, mut doy) = (1970, days);
        loop {
            let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
            let len = if leap { 366 } else { 365 };
            if doy < len {
                break;
            }
            doy -= len;
            y += 1;
        }
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let ml = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        let mut m = 0;
        while doy >= ml[m] {
            doy -= ml[m];
            m += 1;
        }
        format!(
            "{y:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            m + 1,
            doy + 1,
            tod / 3600,
            (tod % 3600) / 60,
            tod % 60
        )
    }

    #[test]
    fn an_empty_store_is_a_failure_not_a_clean_bill() {
        let c = db();
        let r = memory_writes(&c);
        assert_eq!(r.level, Level::Fail);
        assert!(r.fix.is_some(), "a failure with no next step is not actionable");
    }

    #[test]
    fn a_store_nobody_has_written_to_in_days_is_flagged() {
        // The shape of the approve bug: writes "succeed" elsewhere, and the only visible
        // symptom is that nothing new lands here.
        let c = db();
        let old = iso(now_epoch() - 10 * 86_400);
        c.execute("INSERT INTO memories VALUES('1','ds','t',?1,NULL)", [&old]).unwrap();
        assert_eq!(memory_writes(&c).level, Level::Warn);
    }

    #[test]
    fn a_fresh_write_passes() {
        let c = db();
        let now = iso(now_epoch() - 600);
        c.execute("INSERT INTO memories VALUES('1','ds','t',?1,NULL)", [&now]).unwrap();
        let r = memory_writes(&c);
        assert_eq!(r.level, Level::Ok, "{}", r.detail);
        assert!(r.detail.contains('1'), "must report what it saw: {}", r.detail);
    }

    #[test]
    fn dream_switched_off_is_a_decision_not_a_fault() {
        // Conflating "off" with "broken" trains the operator to ignore the report.
        let c = db();
        c.execute("INSERT INTO paos_config VALUES('dream_enabled','0')", []).unwrap();
        assert_eq!(dream(&c).level, Level::Ok);
    }

    #[test]
    fn dream_enabled_but_not_running_is_the_failure_worth_catching() {
        // Exactly the state the cutover left behind: config said on, nothing ran.
        let c = db();
        c.execute("INSERT INTO paos_config VALUES('dream_enabled','1')", []).unwrap();
        c.execute(
            "INSERT INTO meta VALUES('dream.last_run_epoch',?1)",
            [(now_epoch() - 9 * 86_400).to_string()],
        )
        .unwrap();
        c.execute(
            "INSERT INTO meta VALUES('dream.last_ok_epoch',?1)",
            [(now_epoch() - 9 * 86_400).to_string()],
        )
        .unwrap();
        let r = dream(&c);
        assert_eq!(r.level, Level::Fail);
        assert!(r.detail.contains("9d"), "say how stale: {}", r.detail);
    }

    #[test]
    fn a_dream_that_starts_but_never_finishes_is_caught() {
        // "last ran 0d ago" would call this healthy while every run dies halfway.
        let c = db();
        c.execute("INSERT INTO paos_config VALUES('dream_enabled','1')", []).unwrap();
        c.execute("INSERT INTO meta VALUES('dream.last_run_epoch',?1)",
                  [now_epoch().to_string()]).unwrap();
        let r = dream(&c);
        assert_eq!(r.level, Level::Warn);
        assert!(r.detail.contains("never finished"), "{}", r.detail);
    }

    #[test]
    fn a_silent_telegram_bridge_is_a_failure() {
        // The operator cannot see this himself: a wedged bridge looks exactly like a
        // session that is busy, because both mean "no reply".
        let c = db();
        c.execute("INSERT INTO meta VALUES('telegram.last_poll_epoch',?1)",
                  [(now_epoch() - 40 * 60).to_string()]).unwrap();
        let r = telegram(&c);
        assert_eq!(r.level, Level::Fail);
        assert!(r.detail.contains("40m"), "{}", r.detail);
    }

    #[test]
    fn a_live_bridge_passes_and_a_brief_gap_is_tolerated() {
        let c = db();
        c.execute("INSERT INTO meta VALUES('telegram.last_poll_epoch',?1)",
                  [(now_epoch() - 60).to_string()]).unwrap();
        assert_eq!(telegram(&c).level, Level::Ok);
    }

    #[test]
    fn an_unconfigured_bridge_warns_rather_than_failing() {
        // Not every machine has Telegram wired up; that is a setup state, not a fault.
        assert_eq!(telegram(&db()).level, Level::Warn);
    }

    #[test]
    fn swap_parses_the_real_sysctl_format() {
        // Verbatim from this machine. The first parser trimmed from the END of the line
        // and matched nothing, so swap rendered as a permanently reassuring "unreadable"
        // — a sensor that always says fine is worse than no sensor.
        let (used, total) =
            parse_swap("total = 9216.00M  used = 8393.00M  free = 823.00M  (encrypted)")
                .expect("must parse");
        assert_eq!(total, 9216.0);
        assert_eq!(used, 8393.0);
    }

    #[test]
    fn swap_handles_gigabyte_units_and_refuses_nonsense() {
        // A bigger machine reports G, and scaling it wrong would silently mis-scale the
        // percentage rather than fail.
        let (used, total) = parse_swap("total = 16.00G  used = 8.00G  free = 8.00G").unwrap();
        assert_eq!(total, 16384.0);
        assert_eq!(used, 8192.0);
        assert!(parse_swap("").is_none());
        assert!(parse_swap("garbage without keys").is_none());
    }

    #[test]
    fn a_swap_failure_names_what_is_holding_the_memory() {
        // "swap is full" without "OrbStack is holding 6 GB" is a fact you cannot act on.
        if let Some(hog) = top_memory_hog() {
            assert!(hog.contains("GB"), "{hog}");
        }
    }

    #[test]
    fn an_empty_or_absent_spool_is_healthy() {
        let tmp = std::env::temp_dir().join("paos-spool-none-does-not-exist");
        assert_eq!(spool_at(&tmp).level, Level::Ok, "no spool dir yet is normal, not a fault");
    }

    #[test]
    fn a_write_paosd_could_not_parse_is_a_failure_not_a_silent_drop() {
        // .bad files are KEPT on purpose — each one is a fact a session believed it
        // stored. Keeping them without telling anyone would be the same silent loss.
        let tmp = std::env::temp_dir().join("paos-spool-bad-test");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("x.bad"), b"{").unwrap();
        let r = spool_at(&tmp);
        std::fs::remove_dir_all(&tmp).ok();
        assert_eq!(r.level, Level::Fail);
        assert!(r.detail.contains("could not parse"), "{}", r.detail);
    }

    #[test]
    fn a_fresh_backlog_is_draining_not_broken() {
        // The drain runs on a 10-minute tick, so a file that just appeared is normal.
        let tmp = std::env::temp_dir().join("paos-spool-new-test");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("1.json"), b"{}").unwrap();
        let r = spool_at(&tmp);
        std::fs::remove_dir_all(&tmp).ok();
        assert_eq!(r.level, Level::Ok, "{}", r.detail);
    }

    #[test]
    fn a_queue_nobody_empties_is_flagged() {
        let c = db();
        c.execute(
            "INSERT INTO memory_proposals VALUES(1,'pending',?1)",
            [iso(now_epoch() - 30 * 86_400)],
        )
        .unwrap();
        assert_eq!(review_queue(&c).level, Level::Warn);
    }

    #[test]
    fn timestamps_parse_in_both_formats_this_database_contains() {
        // Python wrote "2026-07-30 12:00:00.123", Rust writes "2026-07-30T12:00:00Z".
        // Demanding one format would silently report 0h for every row of the other.
        let epoch = now_epoch() - 5 * 3600;
        let t = iso(epoch);
        assert_eq!(hours_since_iso(&t), 5);
        assert_eq!(hours_since_iso(&t.replace('T', " ").replace('Z', ".500")), 5);
        assert_eq!(hours_since_iso("nonsense"), 0);
    }

    #[test]
    fn only_a_blocked_session_is_urgent() {
        // A stale nightly job must not buzz the phone as loudly as "memory is broken";
        // that is how an alert channel gets muted.
        let mk = |name, level| Check { name, level, detail: String::new(), fix: None };
        assert!(mk("telegram", Level::Fail).is_urgent());
        assert!(mk("memory writes", Level::Fail).is_urgent());
        assert!(!mk("nightly dream", Level::Fail).is_urgent());
        assert!(!mk("review queue", Level::Fail).is_urgent());
        // A warning is never urgent: WARN is for decisions and setup states.
        assert!(!mk("telegram", Level::Warn).is_urgent());
    }

    #[test]
    fn render_summarises_and_shows_fixes() {
        let checks = vec![
            Check { name: "a", level: Level::Ok, detail: "fine".into(), fix: None },
            Check {
                name: "b",
                level: Level::Fail,
                detail: "broken".into(),
                fix: Some("do the thing".into()),
            },
        ];
        let out = render(&checks).join("\n");
        assert!(out.contains("do the thing"));
        assert!(out.contains("1 of 2 checks need attention"), "{out}");
    }

    #[test]
    fn cumulative_cpu_time_parses_both_ps_formats() {
        // THE NUMBER THE WHOLE CHECK TURNS ON. ps prints MM:SS.ss under an hour and
        // HH:MM:SS over it, and the incident that motivated this read "432:32.66" — which
        // is 432 MINUTES in the two-field form, not 432 hours.
        assert_eq!(cpu_minutes("432:32.66").map(|m| m.round()), Some(433.0));
        assert_eq!(cpu_minutes("0:00.13").map(|m| (m * 100.0).round()), Some(0.0));
        assert_eq!(cpu_minutes("01:30:00").map(|m| m.round()), Some(90.0));
        assert_eq!(cpu_minutes("garbage"), None);
        assert_eq!(cpu_minutes(""), None);
    }

    #[test]
    fn only_build_and_test_artefacts_are_candidates() {
        // A browser or a VM legitimately accumulates hours. Flagging raw "high CPU" would
        // fire constantly and be learned-around, which is worse than not flagging at all.
        assert!(is_build_artifact("/x/paos/target/debug/deps/paos-6da1dcf77abf0dc3"));
        assert!(is_build_artifact("/x/target/release/paosd"));
        assert!(is_build_artifact("cargo"));
        assert!(is_build_artifact("rustc"));
        assert!(is_build_artifact("/x/node_modules/.bin/jest"));
        assert!(!is_build_artifact("/Applications/OrbStack.app/Contents/MacOS/OrbStack"));
        assert!(!is_build_artifact("WindowServer"));
        assert!(!is_build_artifact("/usr/bin/ssh"));
        // paosd itself lives in ~/.local/bin, not a target dir, so the daemon cannot
        // report itself as a runaway.
        assert!(!is_build_artifact("/Users/x/.local/bin/paosd"));
    }

    #[test]
    fn the_threshold_sits_above_a_real_release_build() {
        // A full `cargo build --release` on this tree is ~40s of wall clock and well under
        // a minute of CPU per core-second budget; the incident had 432 minutes. A
        // threshold near a legitimate build would make this a nuisance check.
        assert!(RUNAWAY_CPU_MINUTES >= 10.0, "too tight — real builds would trip it");
        assert!(RUNAWAY_CPU_MINUTES <= 60.0, "too loose — an orphan runs for hours unseen");
    }


    #[test]
    fn a_path_containing_spaces_is_not_truncated() {
        // Real ps output on this machine: OrbStack's COMM contains spaces. Taking only the
        // next whitespace token would cut a path at the first space, so a build artefact
        // under a directory with a space in it would stop matching and the runaway would
        // go unreported — the check failing silently, which is the failure it exists for.
        assert!(is_build_artifact("/Users/a b/proj/target/debug/deps/test-123"));
        assert!(!is_build_artifact("/Applications/OrbStack.app/Contents/MacOS/OrbStack Helper"));
    }


    #[test]
    fn a_stale_registration_is_one_whose_directory_is_gone() {
        // The scan reads <repo>/.git/worktrees/<name>/gitdir, which points at
        // "<worktree>/.git". A registration is STALE when that worktree directory no
        // longer exists — git still counts it, and the sandbox still embeds a deny path
        // for it. Live worktrees are somebody's work and are deliberately not counted.
        // A unique dir under TMPDIR. mktemp is denied in the agent sandbox and
        // std has no temp-dir helper, so build the path from the test's own name.
        let d = std::env::temp_dir().join("paos-doctor-worktree-test");
        let _ = std::fs::remove_dir_all(&d);
        let wt = d.join("repo").join(".git").join("worktrees");
        std::fs::create_dir_all(wt.join("gone")).unwrap();
        std::fs::create_dir_all(wt.join("alive")).unwrap();
        let live = d.join("live-worktree");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::write(wt.join("gone").join("gitdir"),
                       format!("{}/.git\n", d.join("deleted-worktree").display())).unwrap();
        std::fs::write(wt.join("alive").join("gitdir"),
                       format!("{}/.git\n", live.display())).unwrap();

        // Read them back the way the scan does, asserting the CLASSIFICATION rather than
        // driving stale_worktrees(), which walks $HOME and cannot be pointed at a fixture.
        let classify = |name: &str| -> bool {
            let g = std::fs::read_to_string(wt.join(name).join("gitdir")).unwrap();
            let dir = g.trim().trim_end_matches("/.git").to_string();
            !std::path::Path::new(&dir).exists()
        };
        assert!(classify("gone"), "a missing worktree dir must count as stale");
        assert!(!classify("alive"), "a live worktree must NOT count");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_stale_limit_sits_between_normal_churn_and_the_failure() {
        // ~15 live worktrees on this machine; 228 registered is what broke a session.
        assert!(STALE_WORKTREE_LIMIT > 15, "would fire on normal churn");
        assert!(STALE_WORKTREE_LIMIT < 200, "would only fire after the damage");
    }


    #[test]
    fn the_write_staleness_threshold_is_above_every_real_quiet_period() {
        // Measured over 38 days of this store's own history: longest gap between
        // consecutive writes 39.1 h, next 37.3 h. A threshold at or below those would fire
        // on a genuinely quiet weekend and be learned around; one far above them gives a
        // broken write path days of cover. Both failures are worse than the check.
        assert!(STALE_WRITE_HOURS > 40, "would fire on a real quiet period (max seen 39.1h)");
        assert!(STALE_WRITE_HOURS <= 48, "too loose — a broken write path stays hidden");
    }

}
