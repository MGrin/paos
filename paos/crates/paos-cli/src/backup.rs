//! `paos backup` — snapshot, status, and the restore procedure.
//!
//! The logic lives in `paos_store::backup` so the daemon calls it directly instead of
//! shelling this binary. Only the presentation is here.

use paos_store::backup as bk;
use std::path::PathBuf;

fn dest(args: &[String]) -> PathBuf {
    args.iter()
        .position(|a| a == "--dest")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(bk::default_dest)
}

pub fn run(positional: &[String], args: &[String]) -> i32 {
    match positional.get(1).map(String::as_str).unwrap_or("run") {
        "run" => cmd_run(&dest(args)),
        "status" => cmd_status(&dest(args)),
        "restore" => cmd_restore(&dest(args)),
        other => {
            eprintln!("unknown backup subcommand: {other}\n\
                       usage: paos backup [run | status | restore] [--dest <dir>]");
            2
        }
    }
}

fn cmd_run(dest: &std::path::Path) -> i32 {
    let out = dest.join(bk::stamped_name(&super::utc_stamp()));
    match bk::snapshot(&paos_store::db_path(), &out) {
        Err(e) => {
            eprintln!("backup FAILED: {e}");
            1
        }
        Ok(msg) => {
            let removed = bk::prune(dest, bk::KEEP_DAILY);
            println!("backed up to {} ({msg})", out.display());
            if removed > 0 {
                println!("  pruned {removed} old snapshot(s), keeping {}", bk::KEEP_DAILY);
            }
            if !bk::is_offsite(dest) {
                println!("  NOTE: {} is on this machine. A backup on the same disk does not survive",
                         dest.display());
                println!("        the failure it exists for — set PAOS_BACKUP_DIR to somewhere off-machine.");
            }
            0
        }
    }
}

fn cmd_status(dest: &std::path::Path) -> i32 {
    let Some(newest) = bk::latest(dest) else {
        println!("NO BACKUPS in {}", dest.display());
        return 1;
    };
    let meta = std::fs::metadata(&newest).ok();
    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let age_h = meta
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.elapsed().ok())
        .map(|d| d.as_secs_f64() / 3600.0)
        .unwrap_or(f64::MAX);
    println!("{}\n  {:.1} MB, {:.1} h old", newest.display(), size as f64 / 1e6, age_h);
    println!("  {} snapshot(s) retained", bk::snapshots(dest).len());
    // Non-zero past 48h so `doctor` and cron can treat a stale backup as a failure
    // rather than something a human has to notice.
    if age_h < 48.0 { 0 } else { 1 }
}

fn cmd_restore(dest: &std::path::Path) -> i32 {
    // Deliberately prints rather than does. Restoring over a live database while paosd
    // holds it open is how you turn one bad day into two.
    let newest = bk::latest(dest)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<snapshot>".into());
    println!("restore is deliberately manual — paosd holds the database open:\n");
    println!("  launchctl bootout gui/$(id -u)/ai.paos.daemon");
    println!("  cp ~/.paos/paos.db ~/.paos/paos.db.before-restore");
    println!("  gunzip -c {newest} > ~/.paos/paos.db");
    println!("  rm -f ~/.paos/paos.db-wal ~/.paos/paos.db-shm   # stale WAL vs a new file");
    println!("  launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/ai.paos.daemon.plist");
    println!("  paosctl doctor");
    0
}
