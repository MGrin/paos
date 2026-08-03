//! Claude account usage, as a first-class paos facet.
//!
//! Reads the usage cache the poller writes and owns the *presentation*.
//!
//! It used to SHELL two Python helpers — `dash-claude-usage` to read one JSON file, and
//! `claude-acct use` to switch. Both are now in-process: the daemon was paying a python3
//! interpreter boot per read, six times over, to parse a file it could open itself, and
//! the switch RULES lived in Python for those callers and in Rust for the rest.
//!
//! Why it belongs in paos: knowing you are about to hit a weekly cap is operational
//! information about the fleet, in the same class as "a session is blocked". It should
//! reach the phone the same way.

#[derive(Debug, Clone, PartialEq)]
pub struct Account {
    pub slot: String,
    pub email: String,
    pub active: bool,
    /// Percent of the 5-hour window consumed. `None` means UNKNOWN — the window was
    /// absent from the cache, which is what an unpollable account looks like.
    ///
    /// **Unknown is not zero, and the difference decides where the fleet goes.** These
    /// were `f64` defaulting to 0.0, so an account whose poll failed parsed as 0% used
    /// and therefore as the emptiest account on the machine. Measured: with a healthy
    /// account at 20%/30% available, the picker chose the one that had just returned
    /// HTTP 401. The Python excluded such rows outright.
    pub five_hour: Option<f64>,
    /// Percent of the 7-day window consumed — the one that actually bites.
    pub seven_day: Option<f64>,
    /// Why this account could not be polled, if it could not be.
    pub error: Option<String>,
    pub seven_day_resets: Option<String>,
    pub five_hour_resets: Option<String>,
    /// Percent of the 24h window, and when it was polled — carried for the dashboard.
    pub polled_at: Option<i64>,
}

/// Where a percentage sits, per window.
///
/// The two windows have DIFFERENT thresholds on purpose. The weekly window is the one
/// that stops work, so it warns early — 60%, because two-thirds of a week consumed may
/// still have days of work to fit into what remains. The 5-hour window refills on its
/// own, so warning at the same level would cry wolf several times a day.
pub const BAND_5H_WARNING: f64 = 75.0;
pub const BAND_5H_CRITICAL: f64 = 95.0;
pub const BAND_7D_WARNING: f64 = 60.0;
pub const BAND_7D_CRITICAL: f64 = 90.0;

pub fn band(window: &str, pct: f64) -> &'static str {
    let (warn, crit) = if window == "fiveHour" {
        (BAND_5H_WARNING, BAND_5H_CRITICAL)
    } else {
        (BAND_7D_WARNING, BAND_7D_CRITICAL)
    };
    if pct >= crit { "critical" } else if pct >= warn { "warning" } else { "ok" }
}

impl Account {
    /// The 5-hour percentage **for display**, with unknown rendered as 0.
    ///
    /// Only ever for rendering. Anything that DECIDES — the picker above all — must read
    /// the `Option` and treat unknown as "cannot be used", never as headroom.
    pub fn shown_5h(&self) -> f64 {
        self.five_hour.unwrap_or(0.0)
    }
    /// The 7-day percentage **for display**. Same rule as `shown_5h`.
    pub fn shown_7d(&self) -> f64 {
        self.seven_day.unwrap_or(0.0)
    }
    /// A weekly window this full is worth surfacing before it stops work.
    pub fn is_critical(&self) -> bool {
        self.shown_7d() >= 90.0
    }
    /// 60%, not 70%: this is a WEEKLY window, so two-thirds consumed is already worth
    /// a heads-up — there may be days of work left to fit into what remains. A live
    /// account at 67% rendering green was the bug this threshold fixes.
    pub fn is_warning(&self) -> bool {
        self.shown_7d() >= 60.0 && !self.is_critical()
    }
}

/// Read the usage snapshot. Returns None when the helper is absent or silent, so a
/// machine without the Claude tooling simply has no accounts rather than an error.
/// Where the poller writes the usage cache.
/// ONE definition of where the cluster lives, not two.
///
/// This built its path from `HOME` directly while `slots::config_dir()` honours
/// `PAOS_ACCOUNTS_DIR`. Under that override the poller would have WRITTEN the cache to one
/// directory and every reader would have looked in another — the two halves silently
/// disagreeing about the same file, which is only invisible because nothing sets the
/// override yet.
pub fn cache_path() -> std::path::PathBuf {
    crate::slots::config_dir().join("usage.json")
}

/// A cache older than this is stale. The poller runs every 180s, so 15 minutes is five
/// missed polls — long enough not to flap, short enough that a dead poller is visible.
pub const STALE_AFTER_SECS: i64 = 15 * 60;

/// The raw cache, with `stale` set the way `dash-claude-usage` sets it.
///
/// Reads the FILE. This used to shell a 21-line Python script that did exactly this —
/// `paos accounts list` paid a python3 process (~37ms of interpreter boot) to read one
/// JSON file and add one boolean. The widget still calls that script every 5 seconds;
/// this is the same contract, in-process.
pub fn read_cache_at(path: &std::path::Path, now: i64) -> serde_json::Value {
    let Ok(text) = std::fs::read_to_string(path) else {
        // Unreadable is STALE, not empty: an empty account list renders as a healthy
        // "no accounts" rather than "I cannot see".
        return serde_json::json!({ "stale": true });
    };
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return serde_json::json!({ "stale": true });
    };
    let polled = v.get("polledAt").and_then(|x| x.as_i64()).unwrap_or(0);
    if now - polled > STALE_AFTER_SECS {
        if let Some(o) = v.as_object_mut() {
            o.insert("stale".into(), serde_json::Value::Bool(true));
        }
    }
    v
}

/// Accounts straight from the cache file, no subprocess.
pub fn snapshot_local() -> Option<Vec<Account>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let v = read_cache_at(&cache_path(), now);
    // A stale-only object carries no accounts; report "cannot see" rather than "none".
    if v.get("accounts").is_none() { return None }
    parse(&v.to_string())
}

/// Parse `dash-claude-usage` JSON.
pub fn parse(body: &str) -> Option<Vec<Account>> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let items = v.get("accounts")?.as_array()?;
    let mut out = Vec::new();
    for a in items {
        // Absent stays absent. `unwrap_or(0.0)` here is what made an unpollable
        // account read as the emptiest one on the machine.
        let pct = |key: &str| -> Option<f64> {
            a.get(key).and_then(|x| x.get("util")).and_then(|x| x.as_f64())
        };
        out.push(Account {
            slot: a.get("slot").and_then(|x| x.as_str()).unwrap_or("?").to_string(),
            email: a.get("email").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            active: a.get("active").and_then(|x| x.as_bool()).unwrap_or(false),
            error: a.get("error").and_then(|x| x.as_str()).map(str::to_string),
            five_hour: pct("fiveHour"),
            seven_day: pct("sevenDay"),
            seven_day_resets: a
                .get("sevenDay")
                .and_then(|x| x.get("resetsAt"))
                .and_then(|x| x.as_str())
                .map(str::to_string),
            five_hour_resets: a
                .get("fiveHour")
                .and_then(|x| x.get("resetsAt"))
                .and_then(|x| x.as_str())
                .map(str::to_string),
            polled_at: v.get("polledAt").and_then(|x| x.as_i64()),
        });
    }
    // Worst first: the account about to stop work is the one to see, whether this is read
    // on a phone, in the CLI or on the dashboard. Sorting HERE rather than in each
    // renderer is what keeps the three surfaces agreeing.
    out.sort_by(|a, b| b.shown_7d().total_cmp(&a.shown_7d()));
    Some(out)
}

/// One account as the dashboard's JSON. Field names match what the Settings and Accounts
/// pages already read, so this is a drop-in for the Python that produced them.
pub fn to_json(a: &Account) -> String {
    let q = |o: &Option<String>| match o {
        Some(s) => format!("\"{}\"", s.replace('"', "\\\"")),
        None => "null".into(),
    };
    format!(
        "{{\"slot\":\"{}\",\"email\":\"{}\",\"active\":{},\"fiveHour\":{},\
         \"sevenDay\":{},\"resetsAt\":{},\"fiveHourResetsAt\":{},\
         \"fiveHourBand\":\"{}\",\"sevenDayBand\":\"{}\",\"polledAt\":{}}}",
        a.slot, a.email, a.active, a.shown_5h(), a.shown_7d(),
        q(&a.seven_day_resets), q(&a.five_hour_resets),
        band("fiveHour", a.shown_5h()), band("sevenDay", a.shown_7d()),
        a.polled_at.map(|p| p.to_string()).unwrap_or_else(|| "null".into()))
}

/// Render for a terminal: column-aligned, no chat chrome.
///
/// Deliberately NOT the same as `render`. That one is written for a phone and carries a
/// header and a `/switch` hint that make no sense at a shell prompt — and the CLI output
/// is what a session reads, so it stays byte-compatible with the Python it replaces.
pub fn render_cli(accounts: &[Account]) -> String {
    if accounts.is_empty() {
        return "no Claude accounts configured".into();
    }
    let mut lines = vec![];
    for a in accounts {
        let health = match band("sevenDay", a.shown_7d()) {
            "critical" => "🔴", "warning" => "🟡", _ => "🟢",
        };
        let resets = a.seven_day_resets.as_deref()
            .map(|r| format!(" · resets {}", r.chars().take(10).collect::<String>()))
            .unwrap_or_default();
        lines.push(format!("{} {} {:<24} 7d {:3.0}%  5h {:3.0}%{}",
                           if a.active { "▶" } else { " " }, health, a.slot,
                           a.shown_7d(), a.shown_5h(), resets));
    }
    if let Some(worst) = accounts.first() {
        if band("sevenDay", worst.shown_7d()) == "critical" {
            lines.push(String::new());
            lines.push(format!("⚠ {} is at {:.0}% of its weekly limit",
                               worst.slot, worst.shown_7d()));
        }
    }
    lines.join("\n")
}

/// Render for a phone: one line per account, worst first, so the thing that will stop
/// work is the first thing read.
pub fn render(accounts: &[Account]) -> String {
    if accounts.is_empty() {
        return "no Claude accounts configured".into();
    }
    let mut sorted: Vec<&Account> = accounts.iter().collect();
    sorted.sort_by(|a, b| b.shown_7d().total_cmp(&a.shown_7d()));

    let mut lines = vec!["🤖 Claude accounts".to_string()];
    for a in &sorted {
        let mark = if a.active { "▶" } else { " " };
        let health = if a.is_critical() {
            "🔴"
        } else if a.is_warning() {
            "🟡"
        } else {
            "🟢"
        };
        let resets = a
            .seven_day_resets
            .as_deref()
            .map(|r| r.chars().take(10).collect::<String>())
            .map(|d| format!(" · resets {d}"))
            .unwrap_or_default();
        lines.push(format!(
            "{mark} {health} {}  7d {:.0}%  5h {:.0}%{resets}",
            a.slot, a.shown_7d(), a.shown_5h()
        ));
    }
    if let Some(worst) = sorted.first() {
        if worst.is_critical() {
            lines.push(String::new());
            lines.push(format!("⚠ {} is at {:.0}% of its weekly limit", worst.slot, worst.shown_7d()));
        }
    }
    lines.push(String::new());
    lines.push("/switch — rotate to the least-used account".into());
    lines.join("\n")
}

/// Thresholds for the switch decision. Defaults match the Python's DEFAULT_CONFIG.
#[derive(Debug, Clone, Copy)]
pub struct SwitchConfig {
    /// 5-hour burst ceiling.
    pub switch_at: f64,
    /// Weekly ceiling. LOWER than the burst one on purpose — see `decide_switch`.
    pub weekly_switch_at: f64,
    /// A candidate must be below this on the 5-hour window.
    pub target_max: f64,
    /// Seconds between switches.
    pub cooldown: i64,
}

impl Default for SwitchConfig {
    fn default() -> Self {
        // COOLDOWN is 120, not 900. It was 900 under a doc comment claiming these matched
        // the Python's DEFAULT_CONFIG, which they did not — the Python has 120. The
        // verdict harness could not catch it: it passes the SAME config object to both
        // implementations, so it proves the ALGORITHM agrees and says nothing about the
        // DEFAULTS agreeing. There is no config.json on this machine, so the defaults are
        // what actually run.
        Self { switch_at: 95.0, weekly_switch_at: 90.0, target_max: 80.0, cooldown: 120 }
    }
}

impl SwitchConfig {
    /// `~/.config/claude-usage/config.json` over the defaults.
    ///
    /// Ported from `load_config`, which nothing in the Rust had — a machine with a
    /// config.json was being polled and switched against the built-in numbers, silently.
    /// Unknown keys are ignored the same way the Python ignores them, so a typo'd key
    /// leaves the default in place rather than erroring.
    pub fn load_at(path: &std::path::Path) -> SwitchConfig {
        let mut cfg = SwitchConfig::default();
        let Ok(text) = std::fs::read_to_string(path) else { return cfg };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return cfg };
        let num = |k: &str| v.get(k).and_then(|x| x.as_f64());
        if let Some(x) = num("SWITCH_AT") { cfg.switch_at = x }
        if let Some(x) = num("WEEKLY_SWITCH_AT") { cfg.weekly_switch_at = x }
        if let Some(x) = num("TARGET_MAX") { cfg.target_max = x }
        if let Some(x) = num("COOLDOWN") { cfg.cooldown = x as i64 }
        cfg
    }

    pub fn load() -> SwitchConfig {
        SwitchConfig::load_at(&crate::slots::config_dir().join("config.json"))
    }
}

/// Should the fleet move to another account, and which one?
///
/// ONE picker. This replaces `least_used`, which ranked by `min(seven_day)` and applied
/// NO exclusions — not even the ACTIVE account. Measured on live data 2026-07-31: the
/// active account was also the lowest-weekly one, so `paos accounts switch` returned the
/// account you were already on and reported "switched". The Python had the real policy
/// and the Rust had a different one; two pickers in two languages, agreeing by nobody
/// having looked.
///
/// The four properties, all from b45aca44 and all load-bearing:
///
/// 1. EITHER window triggers, and the weekly threshold is LOWER. Reading `fiveHour` alone
///    meant an account at 100% of its WEEKLY limit with a quiet burst window never
///    switched — 13% < 95, decline, every 180s, while work had already stopped. The
///    weekly window ends the day; the 5-hour one only pauses it, and a weekly limit
///    cannot be waited out because it resets on a calendar date.
/// 2. NEVER switch INTO a weekly-exhausted account — it trips again on the next poll and
///    burns a switch for nothing.
/// 3. Rank by the window that TRIPPED. Ranking by 5-hour during a weekly exhaustion picks
///    an account that is idle right now but nearly out of week, which stalls again within
///    the hour.
/// 4. Honour the cooldown, so a flapping account cannot thrash the fleet.
///
/// Returns `(target_slot, reason)`; `None` with the reason when it declines, because
/// "why didn't it switch" is the question actually asked at 3am.
/// A window as the reason string shows it: the Python's rendering, unknown included.
///
/// `{:?}` and not `{}` because the cache stores utils as JSON floats (`100.0`, measured on
/// the live file), and Python renders those as `100.0` where Rust's `Display` gives `100`.
/// The reason is appended to `switches.jsonl`, which is the audit trail read when asking
/// why a switch happened — so it should read the same across the cutover.
///
/// One case this does NOT match: a util stored as a JSON *integer* renders `96` in Python
/// and `96.0` here, because Python's formatting follows the number's type and this does
/// not carry it. That shape cannot come from the live cache — every util in it is a float
/// — so it is left alone rather than carried through `Account` for a cosmetic difference
/// in one string.
fn show(v: Option<f64>) -> String {
    match v {
        Some(v) => format!("{v:?}"),
        None => "None".to_string(),
    }
}

pub fn decide_switch(
    accounts: &[Account],
    cfg: &SwitchConfig,
    last_switch_ts: i64,
    now_ts: i64,
) -> (Option<String>, String) {
    let Some(active) = accounts.iter().find(|a| a.active) else {
        return (None, "no active account".into());
    };
    // An UNKNOWN window cannot trip. A poll that failed says nothing about consumption,
    // and treating it as 0 would mean the active account never trips while it is broken.
    let weekly_trip = active.seven_day.is_some_and(|v| v >= cfg.weekly_switch_at);
    let burst_trip = active.five_hour.is_some_and(|v| v >= cfg.switch_at);
    if !(weekly_trip || burst_trip) {
        return (None, format!(
            "active 5h {} < {}, 7d {} < {}",
            show(active.five_hour), cfg.switch_at, show(active.seven_day), cfg.weekly_switch_at));
    }

    let mut candidates: Vec<(f64, &str)> = accounts
        .iter()
        .filter(|a| !a.active)
        // Property 5: an account that could not be POLLED is not a candidate. It was
        // parsing as 0% used — the emptiest account on the machine — so the picker
        // preferred it over every healthy one. Measured: with a healthy account at
        // 20%/30% available, an account that had just returned HTTP 401 won.
        .filter(|a| a.error.is_none())
        // ...and the same rule stated the other way. This one is the LOAD-BEARING half:
        // mutation-tested, removing the `error` filter above changes no verdict, because
        // an errored row carries no windows and is already excluded here. The `error`
        // filter is kept because the Python checks it explicitly and because a row could
        // in principle carry both an error and stale windows — but it is defence in
        // depth, not the guard that does the work. Said plainly so the next reader does
        // not delete this one believing the other covers it.
        .filter(|a| a.five_hour.is_some_and(|v| v <= cfg.target_max))
        // Property 2. An unknown WEEKLY window does not exclude — the Python only
        // excludes on a weekly figure it actually has.
        .filter(|a| !a.seven_day.is_some_and(|v| v >= cfg.weekly_switch_at))
        // Property 3: rank by whichever window is the problem.
        .map(|a| (if weekly_trip { a.shown_7d() } else { a.shown_5h() }, a.slot.as_str()))
        .collect();
    if candidates.is_empty() {
        return (None, "no candidate with headroom".into());
    }
    if now_ts - last_switch_ts < cfg.cooldown {
        return (None, "cooldown".into());
    }
    candidates.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(b.1)));
    let why = if weekly_trip {
        format!("active 7d {}% >= {}", show(active.seven_day), cfg.weekly_switch_at)
    } else {
        format!("active 5h {}% >= {}", show(active.five_hour), cfg.switch_at)
    };
    (Some(candidates[0].1.to_string()), why)
}

/// The reason `decide_switch` gives when the active account is spent and there is nowhere
/// left to go. Matched, not re-derived, so the two cannot drift apart.
pub const NO_HEADROOM: &str = "no candidate with headroom";

/// Is the machine OUT of Claude capacity — active account tripped, nothing to take it?
///
/// THIS IS THE STATE THAT STALLED THE FLEET ON 2026-08-01 AND SAID NOTHING. The switcher
/// worked correctly twice that day; what it could not do was switch to an account that did
/// not exist. One was weekly-exhausted (7d 100%, resetting two days out), the other two
/// burned their 5-hour windows, and from then on every session simply failed. The operator
/// found out because the whole fleet died, not because anything told them.
///
/// `decide_switch` returning `None` is the NORMAL case — it means "nothing to do" on every
/// healthy poll. Only the REASON separates boredom from an emergency:
///   * "active 5h .. < .., 7d .. < .."   healthy, stay quiet
///   * "cooldown"                        transient, retries in seconds
///   * "no active account"               misconfiguration, not exhaustion
///   * NO_HEADROOM                       EXHAUSTED - worth waking someone
///
/// Derived FROM `decide_switch` rather than reimplementing its thresholds. A second copy
/// would eventually disagree with the picker, and then the alert would lie in whichever
/// direction hurts more - silent during a real stall, or crying wolf until ignored.
pub fn exhaustion(
    accounts: &[Account],
    cfg: &SwitchConfig,
    last_switch_ts: i64,
    now_ts: i64,
) -> Option<String> {
    let (target, reason) = decide_switch(accounts, cfg, last_switch_ts, now_ts);
    if target.is_some() || reason != NO_HEADROOM {
        return None;
    }
    let mut rows: Vec<String> = accounts
        .iter()
        .map(|a| {
            format!(
                "  {}{} - 5h {} / week {}",
                if a.active { "ACTIVE " } else { "" },
                a.slot,
                a.five_hour.map(|v| format!("{v:.0}%")).unwrap_or_else(|| "?".into()),
                a.seven_day.map(|v| format!("{v:.0}%")).unwrap_or_else(|| "?".into()),
            )
        })
        .collect();
    rows.sort();
    Some(format!(
        "NO CLAUDE CAPACITY LEFT - the active account is spent and no other has headroom, \
         so sessions will start failing.\n{}",
        rows.join("\n")
    ))
}



#[cfg(test)]
mod exhaustion_tests {
    use super::*;

    fn acct(slot: &str, active: bool, h5: Option<f64>, d7: Option<f64>) -> Account {
        Account {
            slot: slot.into(), email: format!("{slot}@x"), active,
            five_hour: h5, seven_day: d7, error: None,
            five_hour_resets: None, seven_day_resets: None, polled_at: None,
        }
    }

    #[test]
    fn the_real_2026_08_01_shape_is_reported_as_exhausted() {
        // The exact state that stalled the fleet, from switches.jsonl and usage.json:
        // one account weekly-dead, one 5h-dead, and the active one just tripped.
        let a = vec![
            acct("first_example.com",     false, Some(0.0),   Some(100.0)),
            acct("second_example.com",     false, Some(100.0), Some(37.0)),
            acct("third_example.com", true,  Some(100.0), Some(47.0)),
        ];
        let d = exhaustion(&a, &SwitchConfig::default(), 0, 1_000_000)
            .expect("this is exhaustion and must be reported");
        assert!(d.contains("NO CLAUDE CAPACITY LEFT"), "{d}");
        // The operator needs to see WHICH account is spent and why, or the alert just says
        // "something is wrong" and they have to go dig anyway.
        assert!(d.contains("ACTIVE third_example.com"), "must name the active one: {d}");
        assert!(d.contains("week 100%"), "must show the weekly-dead account: {d}");
    }

    #[test]
    fn a_healthy_machine_is_silent() {
        // THE ASSERTION THAT KEEPS THIS ALERT WORTH READING. It fires on every supervise
        // pass; one false positive an hour and the operator learns to ignore it, which
        // costs more than having no alert at all.
        let a = vec![
            acct("a", true,  Some(13.0), Some(47.0)),
            acct("b", false, Some(0.0),  Some(37.0)),
        ];
        assert!(exhaustion(&a, &SwitchConfig::default(), 0, 1_000_000).is_none());
    }

    #[test]
    fn a_tripped_active_with_somewhere_to_go_is_not_exhaustion() {
        // The switcher will handle this on the next poll. Alerting here would page the
        // operator for the system working as designed.
        let a = vec![
            acct("a", true,  Some(100.0), Some(47.0)),
            acct("b", false, Some(0.0),   Some(20.0)),
        ];
        assert!(exhaustion(&a, &SwitchConfig::default(), 0, 1_000_000).is_none());
    }

    #[test]
    fn no_active_account_is_a_misconfiguration_not_exhaustion() {
        // decide_switch returns None with a DIFFERENT reason. Treating every None as
        // exhaustion is the obvious wrong implementation and would fire constantly.
        let a = vec![acct("a", false, Some(0.0), Some(0.0))];
        assert!(exhaustion(&a, &SwitchConfig::default(), 0, 1_000_000).is_none());
    }

    #[test]
    fn an_unpollable_account_does_not_count_as_headroom() {
        // Same trap the picker had: unknown windows are not zero. If an unreadable account
        // suppressed the alert, the fleet would stall in silence exactly as before.
        let mut broken = acct("broken", false, None, None);
        broken.error = Some("HTTP 401".into());
        let a = vec![acct("a", true, Some(100.0), Some(95.0)), broken];
        assert!(exhaustion(&a, &SwitchConfig::default(), 0, 1_000_000).is_some(),
                "an errored account is not somewhere to go");
    }
}

#[cfg(test)]
mod band_tests {
    use super::*;

    const SAMPLE: &str = r#"{"polledAt":1785477287,"accounts":[
      {"slot":"b","email":"b@x","active":false,
       "fiveHour":{"util":0.0},"sevenDay":{"util":33.0,"resetsAt":"2026-08-04T01:00:00Z"}},
      {"slot":"a","email":"a@x","active":true,
       "fiveHour":{"util":67.0,"resetsAt":"2026-07-31T08:30:00Z"},
       "sevenDay":{"util":94.0,"resetsAt":"2026-08-03T04:00:00Z"}}]}"#;

    #[test]
    fn the_two_windows_use_different_thresholds() {
        // The weekly window stops work, so it warns early. The 5-hour window refills on
        // its own; warning it at 60% would cry wolf several times a day.
        assert_eq!(band("sevenDay", 67.0), "warning");
        assert_eq!(band("fiveHour", 67.0), "ok");
        assert_eq!(band("fiveHour", 96.0), "critical");
        assert_eq!(band("sevenDay", 91.0), "critical");
    }

    #[test]
    fn parse_sorts_worst_first_so_all_three_surfaces_agree() {
        // Sorting in the parser rather than each renderer is what stops the phone, the
        // CLI and the dashboard disagreeing about which account matters.
        let a = parse(SAMPLE).unwrap();
        assert_eq!(a[0].slot, "a");
        assert_eq!(a[1].slot, "b");
    }

    #[test]
    fn parse_carries_the_fields_the_dashboard_reads() {
        let a = &parse(SAMPLE).unwrap()[0];
        assert_eq!(a.five_hour_resets.as_deref(), Some("2026-07-31T08:30:00Z"));
        assert_eq!(a.polled_at, Some(1785477287));
        assert!(a.active);
    }

    #[test]
    fn a_missing_reset_is_null_not_an_empty_string() {
        // The dashboard renders "resets <date>" only when this is non-null; an empty
        // string would print a bare "resets".
        let a = &parse(SAMPLE).unwrap()[1];
        assert!(a.five_hour_resets.is_none());
        assert!(to_json(a).contains("\"fiveHourResetsAt\":null"));
    }

    #[test]
    fn json_is_parseable_and_carries_both_bands() {
        let a = &parse(SAMPLE).unwrap()[0];
        let v: serde_json::Value = serde_json::from_str(&to_json(a)).expect("valid JSON");
        assert_eq!(v["sevenDayBand"], "critical");
        assert_eq!(v["fiveHourBand"], "ok");
    }

    #[test]
    fn the_cli_render_marks_the_active_account_and_warns_on_the_weekly_window() {
        let out = render_cli(&parse(SAMPLE).unwrap());
        assert!(out.contains("▶ 🔴 a"), "{out}");
        assert!(out.contains("is at 94% of its weekly limit"), "{out}");
    }

    #[test]
    fn the_cli_render_has_no_chat_chrome() {
        // `render` is written for a phone and carries a header and a /switch hint. Those
        // make no sense at a shell prompt, and the CLI output is what a session reads.
        let out = render_cli(&parse(SAMPLE).unwrap());
        assert!(!out.contains("/switch"), "{out}");
        assert!(!out.contains("🤖"), "{out}");
    }

    #[test]
    fn no_accounts_is_stated_not_rendered_as_an_empty_list() {
        assert_eq!(render_cli(&[]), "no Claude accounts configured");
    }
}


#[cfg(test)]
mod switch_decision_tests {
    use super::*;

    fn acct(slot: &str, active: bool, five: f64, seven: f64) -> Account {
        Account { slot: slot.into(), email: format!("{slot}@x.com"), active,
                  five_hour: Some(five), seven_day: Some(seven), error: None,
                  seven_day_resets: None, five_hour_resets: None, polled_at: None }
    }

    /// An account whose poll FAILED: no windows at all, which is what `error_row` writes.
    fn broken(slot: &str) -> Account {
        Account { slot: slot.into(), email: format!("{slot}@x.com"), active: false,
                  five_hour: None, seven_day: None,
                  error: Some("HTTPError: HTTP Error 401: revoked".into()),
                  seven_day_resets: None, five_hour_resets: None, polled_at: None }
    }
    /// The REAL defaults, not a second copy of them.
    ///
    /// This was a `const` with its own literals, which is how COOLDOWN sat at 900 against
    /// the Python's 120 without a single test noticing: the suite agreed with itself. The
    /// cooldown test used a 50-second gap, which declines under either value.
    fn cfg() -> SwitchConfig { SwitchConfig::default() }

    #[test]
    fn a_healthy_account_does_not_switch() {
        let a = [acct("a", true, 50.0, 10.0), acct("b", false, 10.0, 10.0)];
        assert_eq!(decide_switch(&a, &cfg(), 0, 10_000).0, None);
    }

    #[test]
    fn the_defaults_are_derived_from_the_python_not_remembered() {
        // DERIVE THE CHECK FROM BOTH SOURCES. These defaults carried a comment saying
        // they matched the Python's DEFAULT_CONFIG while COOLDOWN was 900 against the
        // Python's 120, and nothing noticed — the verdict harness passes one config to
        // both sides, so it compares the algorithm and never the defaults.
        let py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../dotfiles/.local/bin/claude_accounts.py");
        let Ok(src) = std::fs::read_to_string(&py) else {
            // Expected once the Python is deleted: the Rust becomes the sole definition
            // and there is nothing left to derive from.
            eprintln!("SKIP: {} is gone — Rust is now the only source", py.display());
            return;
        };
        let start = src.find("DEFAULT_CONFIG = {").expect("DEFAULT_CONFIG moved or was renamed");
        let end = src[start..].find('}').expect("unterminated DEFAULT_CONFIG") + start;
        let body = &src[start..end];
        let val = |key: &str| -> f64 {
            let at = body.find(&format!("\"{key}\":")).unwrap_or_else(|| panic!("{key} missing"));
            body[at + key.len() + 3..]
                .trim_start()
                .split(|c: char| !c.is_ascii_digit() && c != '.')
                .next().unwrap()
                .parse().unwrap_or_else(|e| panic!("{key}: {e}"))
        };
        let d = SwitchConfig::default();
        assert_eq!(d.switch_at, val("SWITCH_AT"));
        assert_eq!(d.weekly_switch_at, val("WEEKLY_SWITCH_AT"));
        assert_eq!(d.target_max, val("TARGET_MAX"));
        assert_eq!(d.cooldown as f64, val("COOLDOWN"), "COOLDOWN drifted from the Python");
    }

    #[test]
    fn a_config_file_overrides_the_defaults_and_a_typo_does_not() {
        let dir = std::env::temp_dir().join(format!("paos-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.json");

        assert_eq!(SwitchConfig::load_at(&p).cooldown, 120, "absent file leaves defaults");

        std::fs::write(&p, r#"{"COOLDOWN": 600, "TARGET_MAX": 50, "SWITCH_ATT": 1}"#).unwrap();
        let c = SwitchConfig::load_at(&p);
        assert_eq!(c.cooldown, 600);
        assert_eq!(c.target_max, 50.0);
        assert_eq!(c.switch_at, 95.0, "a typo'd key leaves the default, as in the Python");

        std::fs::write(&p, "{not json").unwrap();
        assert_eq!(SwitchConfig::load_at(&p).cooldown, 120, "corrupt file leaves defaults");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_account_that_could_not_be_polled_is_never_the_target() {
        // THE LIVE BUG, measured before the fix: an errored row carries no windows, those
        // parsed as 0.0, and 0% used is the emptiest account on the machine — so with a
        // healthy account at 20%/30% sitting right there, the picker chose the one that
        // had just returned HTTP 401. The Python excluded errored rows outright and this
        // never showed up in the verdict harness, because thirteen scenarios built from
        // healthy shapes contain no errored rows.
        let a = [acct("live", true, 99.0, 50.0), broken("dead"), acct("healthy", false, 20.0, 30.0)];
        let (target, _) = decide_switch(&a, &cfg(), 0, 10_000);
        assert_eq!(target.as_deref(), Some("healthy"),
                   "an unpollable account must not out-rank a working one");
    }

    #[test]
    fn a_broken_account_is_no_candidate_even_when_it_is_the_only_one() {
        // Declining is correct here. Switching onto an account that cannot authenticate
        // trades a throttled account for a dead one.
        let a = [acct("live", true, 99.0, 50.0), broken("dead")];
        let (target, why) = decide_switch(&a, &cfg(), 0, 10_000);
        assert_eq!(target, None);
        assert_eq!(why, "no candidate with headroom");
    }

    #[test]
    fn a_missing_window_without_an_error_key_is_still_not_headroom() {
        // The API can omit a window it has no data for, so a row can carry a null `util`
        // with no `error` at all. Excluding only on `error` would leave that case parsing
        // as 0% used. Two sources, one conclusion.
        let mut ghost = acct("ghost", false, 0.0, 0.0);
        ghost.five_hour = None;
        ghost.seven_day = None;
        let a = [acct("live", true, 99.0, 50.0), ghost, acct("healthy", false, 20.0, 30.0)];
        assert_eq!(decide_switch(&a, &cfg(), 0, 10_000).0.as_deref(), Some("healthy"));
    }

    #[test]
    fn an_active_account_with_no_readable_windows_does_not_trip() {
        // The mirror image: unknown must not trip the switch either. A poll failure on
        // the ACTIVE account says nothing about its consumption, and a spurious trip
        // would move the fleet off a perfectly good account.
        let mut live = acct("live", true, 0.0, 0.0);
        live.five_hour = None;
        live.seven_day = None;
        let a = [live, acct("healthy", false, 20.0, 30.0)];
        let (target, why) = decide_switch(&a, &cfg(), 0, 10_000);
        assert_eq!(target, None);
        assert_eq!(why, "active 5h None < 95, 7d None < 90",
                   "and the reason says 'None' rather than claiming 0%");
    }

    #[test]
    fn the_reason_renders_percentages_the_way_the_python_did() {
        // The reason is appended to switches.jsonl, the audit trail read when asking why
        // a switch happened. The live cache stores utils as JSON floats (measured:
        // 100.0, 66.0), which Python renders "100.0" where Rust's Display gives "100".
        let a = [acct("a", true, 13.0, 100.0), acct("b", false, 20.0, 33.0)];
        let (target, why) = decide_switch(&a, &cfg(), 0, 10_000);
        assert_eq!(target.as_deref(), Some("b"));
        assert_eq!(why, "active 7d 100.0% >= 90");

        let a = [acct("a", true, 50.0, 10.0), acct("b", false, 10.0, 10.0)];
        assert_eq!(decide_switch(&a, &cfg(), 0, 10_000).1, "active 5h 50.0 < 95, 7d 10.0 < 90");
    }

    #[test]
    fn a_burst_trip_switches_to_the_lowest_five_hour_candidate() {
        let a = [acct("a", true, 96.0, 10.0), acct("b", false, 40.0, 10.0),
                 acct("cc", false, 20.0, 10.0)];
        assert_eq!(decide_switch(&a, &cfg(), 0, 10_000).0.as_deref(), Some("cc"));
    }

    #[test]
    fn the_cooldown_blocks_a_second_switch() {
        let a = [acct("a", true, 96.0, 10.0), acct("b", false, 20.0, 10.0)];
        assert_eq!(decide_switch(&a, &cfg(), 9_950, 10_000).0, None);
        assert_eq!(decide_switch(&a, &cfg(), 9_950, 10_000).1, "cooldown");
    }

    #[test]
    fn the_cooldown_boundary_is_the_pythons_120_seconds() {
        // A gap that STRADDLES the two values, which is what the existing test lacked:
        // it used 50 seconds, and 50 is inside both 120 and the drifted 900. Between them
        // the Rust silently refused switches the Python allowed, for 13 more minutes.
        let a = [acct("a", true, 96.0, 10.0), acct("b", false, 20.0, 10.0)];
        assert_eq!(decide_switch(&a, &cfg(), 10_000 - 119, 10_000).0, None, "still cooling");
        assert_eq!(decide_switch(&a, &cfg(), 10_000 - 120, 10_000).0.as_deref(), Some("b"),
                   "at 120s the Python switches; at the old 900 default this declined");
    }

    #[test]
    fn no_candidate_under_target_max_means_no_switch() {
        let a = [acct("a", true, 96.0, 10.0), acct("b", false, 85.0, 10.0)];
        assert_eq!(decide_switch(&a, &cfg(), 0, 10_000).0, None);
    }

    // ---- the four properties b45aca44 established ----

    #[test]
    fn weekly_exhaustion_triggers_even_when_the_burst_window_is_quiet() {
        // THE BUG: this read fiveHour ONLY, so an account at 100% of its WEEKLY limit
        // with a quiet burst window never switched — 13 < 95, decline, every 180s, while
        // work had already stopped. Observed live: active at 7d 100% / 5h 13% with two
        // idle accounts sitting unused.
        let a = [acct("a", true, 13.0, 100.0), acct("b", false, 20.0, 33.0)];
        let (target, why) = decide_switch(&a, &cfg(), 0, 10_000);
        assert_eq!(target.as_deref(), Some("b"));
        assert!(why.contains("7d"), "the reason must name the window that tripped: {why}");
    }

    #[test]
    fn the_weekly_threshold_is_lower_than_the_burst_one() {
        // Not cosmetic: a weekly limit cannot be waited out — it resets on a calendar
        // date — so it must trip EARLIER than the burst ceiling.
        assert!(cfg().weekly_switch_at < cfg().switch_at);
        let a = [acct("a", true, 50.0, 92.0), acct("b", false, 10.0, 10.0)];
        assert_eq!(decide_switch(&a, &cfg(), 0, 10_000).0.as_deref(), Some("b"),
                   "7d 92 is under the burst ceiling but over the weekly one");
    }

    #[test]
    fn never_switches_into_a_weekly_exhausted_account() {
        // It would trip again on the very next poll and burn a switch for nothing.
        let a = [acct("a", true, 96.0, 10.0), acct("b", false, 10.0, 100.0)];
        assert_eq!(decide_switch(&a, &cfg(), 0, 10_000).0, None);
        assert_eq!(decide_switch(&a, &cfg(), 0, 10_000).1, "no candidate with headroom");
    }

    #[test]
    fn a_weekly_trip_ranks_by_weekly_headroom_not_five_hour() {
        // Ranking by 5h during a weekly exhaustion picks the account that is idle RIGHT
        // NOW but nearly out of week — which stalls again within the hour.
        let a = [acct("a", true, 13.0, 100.0),
                 acct("idle-but-spent", false, 5.0, 85.0),   // best 5h, nearly out of week
                 acct("busy-but-fresh", false, 60.0, 20.0)]; // worse 5h, plenty of week
        assert_eq!(decide_switch(&a, &cfg(), 0, 10_000).0.as_deref(), Some("busy-but-fresh"));
    }

    #[test]
    fn the_active_account_is_never_its_own_replacement() {
        // THE LIVE BUG the old `least_used` had: it ranked by min(seven_day) with NO
        // exclusions, so when the active account was also the lowest-weekly one — measured
        // on this machine 2026-07-31, active at 7d 18% and lowest — `accounts switch`
        // returned the account you were already on and reported "switched".
        let a = [acct("active-and-lowest", true, 96.0, 5.0),
                 acct("other", false, 20.0, 40.0)];
        assert_eq!(decide_switch(&a, &cfg(), 0, 10_000).0.as_deref(), Some("other"));
    }

    #[test]
    fn declining_always_says_why() {
        // "Why didn't it switch" is the question actually asked at 3am, and a bare None
        // cannot answer it.
        for (a, expect) in [
            (vec![acct("a", false, 10.0, 10.0)], "no active account"),
            (vec![acct("a", true, 10.0, 10.0)], "active 5h"),
            (vec![acct("a", true, 96.0, 10.0)], "no candidate with headroom"),
        ] {
            let (t, why) = decide_switch(&a, &cfg(), 0, 10_000);
            assert!(t.is_none());
            assert!(why.contains(expect), "expected {expect:?} in {why:?}");
        }
    }

    #[test]
    fn an_unreadable_cache_is_stale_rather_than_empty() {
        // The distinction that matters: an empty account list renders as a healthy "no
        // accounts", which is what a broken poller would look like on the widget. Stale
        // says "I cannot see", which is the truth.
        let base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
        let missing = std::path::PathBuf::from(base).join("definitely-not-here.json");
        let v = read_cache_at(&missing, 1_000);
        assert_eq!(v["stale"], serde_json::json!(true));
        assert!(v.get("accounts").is_none());

        // Corrupt is treated the same as absent, not parsed into a half-truth.
        let d = std::path::PathBuf::from(std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into()))
            .join(format!("cache-corrupt-{}.json", std::process::id()));
        std::fs::write(&d, b"{not json").unwrap();
        assert_eq!(read_cache_at(&d, 1_000)["stale"], serde_json::json!(true));
    }

    #[test]
    fn the_stale_flag_appears_only_past_the_threshold() {
        let d = std::path::PathBuf::from(std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into()))
            .join(format!("cache-fresh-{}.json", std::process::id()));
        std::fs::write(&d, br#"{"polledAt": 1000, "accounts": []}"#).unwrap();
        // Inside the window: untouched, and crucially NOT marked stale.
        let fresh = read_cache_at(&d, 1000 + STALE_AFTER_SECS - 1);
        assert!(fresh.get("stale").is_none(), "{fresh}");
        // Past it: flagged, and the rest of the payload survives.
        let old = read_cache_at(&d, 1000 + STALE_AFTER_SECS + 1);
        assert_eq!(old["stale"], serde_json::json!(true));
        assert_eq!(old["polledAt"], serde_json::json!(1000));
    }

    #[test]
    fn a_burst_exhausted_account_is_not_offered_however_much_weekly_it_has() {
        // @rustic-otter-2's test from 390f34a8, kept and re-pointed at the unified picker.
        // Handing back an account at 5h 95% is the opposite of what someone pressing that
        // button wants: they want one that can answer NOW.
        let a = [acct("active", true, 96.0, 50.0),
                 acct("burnt", false, 95.0, 10.0), acct("usable", false, 40.0, 5.0)];
        assert_eq!(decide_switch(&a, &cfg(), 0, 10_000).0.as_deref(), Some("usable"));
    }

    #[test]
    fn ties_break_deterministically_so_the_fleet_does_not_flap() {
        let a = [acct("a", true, 96.0, 10.0), acct("z", false, 20.0, 10.0),
                 acct("b", false, 20.0, 10.0)];
        assert_eq!(decide_switch(&a, &cfg(), 0, 10_000).0.as_deref(), Some("b"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real snapshot from this machine, trimmed. Using real shape rather than an
    /// invented one is what catches field-name drift.
    const REAL: &str = r#"{"polledAt":1785363971,"active":"first_example.com","accounts":[
      {"slot":"first_example.com","active":true,"email":"first@example.com",
       "fiveHour":{"util":0.0,"resetsAt":null},
       "sevenDay":{"util":67.0,"resetsAt":"2026-08-03T04:00:00.465993+00:00"}},
      {"slot":"second_example.com","active":false,"email":"second@example.com",
       "fiveHour":{"util":0.0,"resetsAt":null},
       "sevenDay":{"util":100.0,"resetsAt":"2026-07-30T11:00:00.075444+00:00"}},
      {"slot":"third_example.com","active":false,"email":"third@example.com",
       "fiveHour":{"util":0.0,"resetsAt":null},
       "sevenDay":{"util":33.0,"resetsAt":"2026-08-04T00:59:59.545073+00:00"}}]}"#;

    #[test]
    fn parses_the_real_snapshot() {
        // Look up by slot, not by position: `parse` sorts worst-first, and a test that
        // encodes the input order would break every time the real usage shifts.
        let a = parse(REAL).expect("should parse");
        assert_eq!(a.len(), 3);
        let mine = a.iter().find(|x| x.slot == "first_example.com").unwrap();
        assert!(mine.active);
        assert_eq!(mine.seven_day, Some(67.0));
        assert!(a.iter().any(|x| x.seven_day == Some(100.0)));
    }

    #[test]
    fn parse_orders_worst_first() {
        // The account about to stop work is the one to read first, on every surface.
        let a = parse(REAL).unwrap();
        assert_eq!(a[0].seven_day, Some(100.0));
        for w in a.windows(2) {
            assert!(w[0].shown_7d() >= w[1].shown_7d(), "not sorted: {a:?}");
        }
    }

    #[test]
    fn flags_the_account_that_is_about_to_stop_working() {
        let a = parse(REAL).unwrap();
        let at = |p: f64| a.iter().find(|x| x.seven_day == Some(p)).unwrap();
        assert!(at(100.0).is_critical(), "100% weekly must be critical");
        assert!(at(67.0).is_warning(), "67% of a weekly cap must not read as healthy");
    }

    #[test]
    fn warning_and_critical_do_not_overlap() {
        let mk = |p: f64| Account {
            slot: "s".into(), email: String::new(), active: false,
            five_hour: Some(0.0), seven_day: Some(p), error: None, seven_day_resets: None,
            five_hour_resets: None, polled_at: None,
        };
        assert!(!mk(50.0).is_warning() && !mk(50.0).is_critical());
        assert!(mk(67.0).is_warning(), "the real active account sits here");
        assert!(mk(75.0).is_warning() && !mk(75.0).is_critical());
        assert!(mk(95.0).is_critical() && !mk(95.0).is_warning());
    }

    #[test]
    fn worst_account_is_rendered_first() {
        // On a phone the thing about to stop work must be the first line read.
        let out = render(&parse(REAL).unwrap());
        let scani = out.find("second_example.com").unwrap();
        let flare = out.find("third_example.com").unwrap();
        assert!(scani < flare, "100% must sort above 33%:\n{out}");
        assert!(out.contains("⚠"), "a critical account must be called out:\n{out}");
    }

    #[test]
    fn the_active_account_is_marked() {
        let out = render(&parse(REAL).unwrap());
        assert!(out.contains("▶ 🟡 first_example.com"), "{out}");
    }

    #[test]
    fn the_picker_chooses_headroom_on_real_data_and_never_the_active_account() {
        // Was `least_used_picks_the_most_headroom`, asserting min(seven_day) with no
        // exclusions. That policy returned the ACTIVE account whenever it happened to be
        // the lowest-weekly one — measured live on 2026-07-31 — so `accounts switch`
        // reported a switch that never happened. Now goes through decide_switch, which is
        // the same picker the poller uses.
        let a = parse(REAL).unwrap();
        let active = a.iter().find(|x| x.active).map(|x| x.slot.clone());
        // Force a trip so the decision is exercised rather than declined.
        let cfg = SwitchConfig { switch_at: 0.0, ..Default::default() };
        let (target, _why) = decide_switch(&a, &cfg, 0, 10_000);
        let target = target.expect("a candidate exists in the real fixture");
        assert_ne!(Some(target.clone()), active, "must never return the active account");
        assert_eq!(target, "third_example.com");
    }

    #[test]
    fn missing_or_broken_data_degrades_quietly() {
        // A machine without the Claude tooling has no accounts; it is not an error.
        assert!(parse("").is_none());
        assert!(parse("{}").is_none());
        assert!(parse(r#"{"accounts":"nope"}"#).is_none());
        assert_eq!(render(&[]), "no Claude accounts configured");
        // An unreadable cache file is "cannot see", not "none configured" — the two look
        // identical to a renderer and only one of them means the poller is broken.
        let missing = std::path::Path::new("/definitely/not/a/real/cache.json");
        assert_eq!(read_cache_at(missing, 0), serde_json::json!({ "stale": true }));
    }

    #[test]
    fn absent_fields_are_unknown_rather_than_zero() {
        let a = parse(r#"{"accounts":[{"slot":"x"}]}"#).unwrap();
        // UNKNOWN, not 0. Zero is a claim about consumption that nothing measured, and
        // the picker reads it as the emptiest account on the machine.
        assert_eq!(a[0].seven_day, None);
        assert_eq!(a[0].five_hour, None);
        // Display still renders 0, exactly as before — only the DECISION changed.
        assert_eq!(a[0].shown_7d(), 0.0);
        assert!(a[0].seven_day_resets.is_none());
        assert!(!render(&a).is_empty());
    }
}

