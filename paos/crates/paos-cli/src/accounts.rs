//! `paos accounts` — Claude subscription slots and usage.
//!
//! Ported from `accounts_facet.py`, which was a SECOND implementation of what
//! `paos_operator::accounts` already did: two parsers, two renderers, two band
//! thresholds, kept in step by hand. The Telegram path used one and the CLI and dashboard
//! used the other, so a threshold change had to be made twice or the phone and the
//! dashboard would disagree about whether an account was healthy.
//!
//! Local by design: this shells the same helpers the poller uses. Routing it through the
//! daemon would make it unavailable from inside a sandbox for no benefit.

use paos_operator::accounts as acct;


pub fn run(positional: &[String], args: &[String]) -> i32 {
    let sub = positional.get(1).map(String::as_str).unwrap_or("list");
    let json = args.iter().any(|a| a == "--json");
    match sub {
        "list" => list(json),
        // The exact JSON `dash-claude-usage` emits, so the Übersicht widget — which polls
        // every 5 SECONDS — can be repointed at this and the Python reader deleted. Same
        // contract: the raw cache plus `stale`.
        // Prints the switch VERDICT for a cache file, without performing it. Exists so the
        // Rust decision can be driven against the Python's on the same input — the
        // safety-critical property is not "the poller runs" but "it reaches the same
        // verdict", and that cannot be checked by running the poller.
        "decide" => {
            let path = positional.get(2).map(std::path::PathBuf::from)
                .unwrap_or_else(acct::cache_path);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
            let v = acct::read_cache_at(&path, now);
            let Some(list) = acct::parse(&v.to_string()) else {
                println!("none\tunreadable cache"); return 0;
            };
            let last = args.iter().position(|a| a == "--last-switch")
                .and_then(|i| args.get(i + 1)).and_then(|x| x.parse().ok()).unwrap_or(0);
            // `--now` exists so the COOLDOWN can be compared at all. Without it this used
            // wall-clock while the Python harness passed a fixed timestamp, so both sides
            // saw an enormous gap since the last switch and neither ever reached the
            // cooldown branch — which is part of why a 900-vs-120 default drift survived.
            let now = args.iter().position(|a| a == "--now")
                .and_then(|i| args.get(i + 1)).and_then(|x| x.parse().ok()).unwrap_or(now);
            let (t, why) = acct::decide_switch(&list, &acct::SwitchConfig::load(), last, now);
            println!("{}\t{}", t.unwrap_or_else(|| "none".into()), why);
            0
        }
        "raw" => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            println!("{}", acct::read_cache_at(&acct::cache_path(), now));
            0
        }
        "switch" => switch(positional.get(2).map(String::as_str)),

        // --- the verbs that were `claude-acct` -----------------------------
        "slots" => slots_list(),
        "current" => { println!("{}", cur_slot()); 0 }
        "capture" => {
            match paos_operator::switch::capture_current(
                &paos_operator::switch::Live, positional.get(2).map(String::as_str))
            {
                Ok(slot) => { println!("captured: {slot}"); 0 }
                Err(e) => { eprintln!("{e}"); 1 }
            }
        }
        "use" => {
            let Some(slot) = positional.get(2) else {
                eprintln!("usage: paos accounts use <slot>");
                return 2;
            };
            match paos_operator::poll::switch_now(slot, paos_operator::poll::now_epoch()) {
                Ok(()) => { println!("now: {}", cur_slot()); 0 }
                Err(e) => { eprintln!("{e}"); 1 }
            }
        }
        "next" => {
            // Round-robin, NOT the picker: "the one after this" is what stepping through
            // accounts by hand means.
            let Some(next) = paos_operator::slots::next_slot() else {
                eprintln!("no accounts captured");
                return 1;
            };
            match paos_operator::poll::switch_now(&next, paos_operator::poll::now_epoch()) {
                Ok(()) => { println!("now: {}", cur_slot()); 0 }
                Err(e) => { eprintln!("{e}"); 1 }
            }
        }
        "auto" => {
            let paths = paos_operator::poll::Paths::live();
            match positional.get(2).map(String::as_str) {
                Some("on") | Some("off") => {
                    let on = positional.get(2).map(String::as_str) == Some("on");
                    if let Err(e) = paos_operator::poll::set_auto(&paths.auto_flag, on) {
                        eprintln!("{e}");
                        return 1;
                    }
                }
                Some(other) => {
                    eprintln!("usage: paos accounts auto [on|off] (got {other:?})");
                    return 2;
                }
                None => {}
            }
            println!("auto-switch: {}",
                     if paos_operator::poll::auto_enabled(&paths.auto_flag) { "on" } else { "off" });
            0
        }
        "poll" => {
            // The LaunchAgent entry point. Writes the cache and, if auto is on, switches.
            //
            // REFUSED FROM A SANDBOX, and this guard is about the blast radius rather
            // than about the poll failing. Inside an agent session the network is
            // restricted and the keychain is read-only, so every account's fetch errors —
            // and poll_all is fail-soft, so it cheerfully writes a cache in which every
            // row is an error. That file is the live one: the Übersicht widget reads it
            // every 5 seconds, the dashboard and `paos accounts list` read it, and an
            // errored row renders as an account at 0% rather than as broken. One
            // exploratory `paos accounts poll` from a session would therefore blank the
            // usage view for the whole machine until the LaunchAgent's next run.
            //
            // The guard lives HERE and not in run_poll on purpose: run_poll must keep the
            // Python's semantics exactly while the two implementations are still being
            // compared. This is a new CLI verb, so it owes no parity.
            if !paos_operator::slots::keychain_writable() {
                eprintln!("refusing to poll from here: the keychain is read-only in this \
                           process, so every account would fail to refresh and the cache \
                           would be overwritten with errors that every reader on this \
                           machine renders as 0%. Run it in a terminal, or let the \
                           LaunchAgent do it.");
                return 1;
            }
            let paths = paos_operator::poll::Paths::live();
            let cache = paos_operator::poll::run_poll(
                &paos_operator::poll::Live,
                &paos_operator::switch::Live,
                &paths,
                &acct::SwitchConfig::load(),
                paos_operator::poll::now_epoch(),
                &paos_operator::poll::notify,
            );
            if json { println!("{cache}"); }
            0
        }
        other => {
            eprintln!("unknown accounts subcommand: {other}\n\
                       usage: paos accounts [list [--json] | raw | slots | current |\n\
                       \x20                 capture [<slot>] | use <slot> | next |\n\
                       \x20                 switch [<slot>] | auto [on|off] | poll]");
            2
        }
    }
}

fn list(json: bool) -> i32 {
    let Some(accounts) = acct::snapshot_local() else {
        // Exit non-zero so the dashboard can tell "could not read" from "none configured".
        // Printing [] here would show an empty, healthy-looking list for a broken poller.
        eprintln!("could not read Claude usage (is the poller running? {})",
                  acct::cache_path().display());
        return 1;
    };
    if json {
        let body: Vec<String> = accounts.iter().map(acct::to_json).collect();
        println!("[{}]", body.join(","));
    } else {
        println!("{}", acct::render_cli(&accounts));
    }
    0
}

fn switch(slot: Option<&str>) -> i32 {
    let Some(accounts) = acct::snapshot_local() else {
        eprintln!("could not read Claude usage");
        return 1;
    };
    // No slot given: the SAME picker the auto-switch poller uses, not a second one.
    //
    // This used to call `least_used`, which was min(seven_day) with no exclusions — not
    // even the active account. Measured live on 2026-07-31: the active account was also
    // the lowest-weekly one, so this reported "switched to second_example.com" while
    // changing nothing. Two pickers in two languages, and the divergence survived because
    // nobody had run them against the same data.
    //
    // `switch_at: 0.0` forces the decision: an explicit `paos accounts switch` is a
    // deliberate request, so it should not silently decline because the active account is
    // still under its ceiling. Every OTHER rule still applies — never the active account,
    // never a weekly-exhausted one, never one over TARGET_MAX.
    let target = match slot {
        Some(s) => s.to_string(),
        None => {
            let cfg = acct::SwitchConfig { switch_at: 0.0, cooldown: 0,
                                           ..Default::default() };
            match acct::decide_switch(&accounts, &cfg, 0, 0) {
                (Some(t), _) => t,
                (None, why) => {
                    // Say WHY rather than "no accounts configured", which was wrong for
                    // every reason except the literal absence of accounts.
                    eprintln!("not switching: {why}");
                    return 1;
                }
            }
        }
    };
    // In-process, through the ONE switcher. This shelled `claude-acct use`, so the
    // re-stash of the outgoing credential, the rollback on a failed identity write and
    // the read-only-keychain refusal lived in Python for this caller and in Rust for the
    // others.
    match paos_operator::poll::switch_now(&target, paos_operator::poll::now_epoch()) {
        Ok(()) => { println!("now: {}", cur_slot()); 0 }
        Err(e) => { eprintln!("{e}"); 1 }
    }
}

fn cur_slot() -> String {
    paos_operator::slots::current_slot().unwrap_or_else(|| "(unknown)".into())
}

/// `claude-acct list` — slot names, the live one marked, and the auto-switch flag.
///
/// Deliberately NOT `paos accounts list`, which renders usage bands for a human. This is
/// the SLOT view: which credentials are stashed and which is in force. They answer
/// different questions and the Python kept them as different commands.
fn slots_list() -> i32 {
    let slots = paos_operator::slots::list_slots();
    let cur = paos_operator::slots::current_slot();
    let cache = acct::read_cache_at(&acct::cache_path(), paos_operator::poll::now_epoch());
    let util = |slot: &str| -> String {
        cache.get("accounts").and_then(|a| a.as_array())
            .and_then(|arr| arr.iter().find(|r| r.get("slot").and_then(|x| x.as_str()) == Some(slot)))
            .and_then(|r| r.get("fiveHour")).and_then(|w| w.get("util")).and_then(|u| u.as_f64())
            .map(|u| format!("{u:?}")).unwrap_or_else(|| "-".into())
    };
    if slots.is_empty() {
        println!("no accounts captured yet. Log into each account, then: paos accounts capture");
    }
    for s in &slots {
        let mark = if Some(s.as_str()) == cur.as_deref() { "*" } else { " " };
        println!("{mark} {s:24}  5h={}%", util(s));
    }
    let paths = paos_operator::poll::Paths::live();
    println!("auto-switch: {}",
             if paos_operator::poll::auto_enabled(&paths.auto_flag) { "on" } else { "off" });
    0
}
