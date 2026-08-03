//! Snapshots of the one file on this machine that cannot be rebuilt.
//!
//! `~/.paos/paos.db` holds every durable memory, bus message and session record here, and
//! nothing backed it up — Time Machine has no destination configured on this machine.
//!
//! Ported from `backup_facet.py`. The port matters more than most: the daemon's ONLY
//! backup path was shelling the Python CLI, so a Python that failed to import took the
//! backups with it, silently, in exactly the way this system tends to fail. paosd now
//! calls this directly.
//!
//! Compression shells `gzip` rather than adding a compression crate. Consistent with the
//! rest of the workspace, which already shells `du`, `df`, `git` and `curl` and keeps its
//! dependency list to serde, serde_json and rusqlite.

use crate::{Result, StoreError};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

/// How many daily snapshots to retain.
pub const KEEP_DAILY: usize = 14;

/// Where snapshots go, in preference order.
///
/// Google Drive first, because a backup on the same disk does not survive the failure it
/// exists for. The ACCOUNT root is not writable — Drive exposes "My Drive" beneath it and
/// mkdir at the root fails with EACCES, verified on this machine — so descend into it
/// when present.
pub fn default_dest() -> PathBuf {
    if let Ok(d) = std::env::var("PAOS_BACKUP_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
    let cloud = home.join("Library/CloudStorage");
    if let Ok(entries) = std::fs::read_dir(&cloud) {
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("GoogleDrive-"))
            })
            .collect();
        dirs.sort();
        if let Some(d) = dirs.first() {
            let mine = d.join("My Drive");
            let base = if mine.is_dir() { mine } else { d.clone() };
            return base.join("paos-backups");
        }
    }
    // Better a local copy than none — callers say so, because it does not survive a dead
    // disk and a backup nobody questions is worse than no backup.
    home.join(".paos-backups")
}

pub fn is_offsite(dest: &Path) -> bool {
    dest.to_string_lossy().contains("CloudStorage")
}

/// A consistent copy of a LIVE database, verified before it counts.
///
/// `VACUUM INTO` rather than a file copy: the database is open and being written, and a
/// byte copy of a WAL-mode file mid-transaction is not a database.
pub fn snapshot(src: &Path, out: &Path) -> Result<String> {
    if !src.exists() {
        return Err(StoreError::Backup(format!("no database at {}", src.display())));
    }
    // Unique per CALL, not per process. `paos-backup-<pid>` collides when two snapshots
    // overlap — the daemon's timer and a manual `paos backup run` — and each one deletes
    // the other's working directory on the way out. Caught by two tests in one process
    // doing exactly that.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmpdir = std::env::temp_dir()
        .join(format!("paos-backup-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmpdir);
    std::fs::create_dir_all(&tmpdir)
        .map_err(|e| StoreError::Backup(format!("tempdir: {e}")))?;
    let tmp = tmpdir.join("snap.db");

    let result = (|| -> Result<String> {
        {
            let conn = Connection::open_with_flags(src, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            // VACUUM INTO refuses to overwrite, so tmp must not exist yet.
            conn.execute("VACUUM INTO ?1", [tmp.to_string_lossy().as_ref()])?;
        }
        let (verdict, facts): (String, i64) = {
            let check = Connection::open(&tmp)?;
            let v: String = check.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
            let n: i64 = check.query_row(
                "SELECT COUNT(*) FROM memories WHERE superseded IS NULL", [], |r| r.get(0))?;
            (v, n)
        };
        if verdict != "ok" {
            return Err(StoreError::Backup(format!("snapshot failed integrity_check: {verdict}")));
        }
        if facts == 0 {
            // An empty snapshot of a non-empty store means something is very wrong, and
            // rotating it in could push the real backups out of the retention window.
            return Err(StoreError::Backup(
                "snapshot contains no memories — refusing to keep it".into()));
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| StoreError::Backup(format!("mkdir {}: {e}", parent.display())))?;
        }
        gzip_to(&tmp, out)?;
        let size = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
        Ok(format!("{facts} facts, {:.1} MB", size as f64 / 1e6))
    })();

    let _ = std::fs::remove_dir_all(&tmpdir);
    result
}

fn gzip_to(src: &Path, out: &Path) -> Result<()> {
    let f = std::fs::File::create(out)
        .map_err(|e| StoreError::Backup(format!("create {}: {e}", out.display())))?;
    let status = std::process::Command::new("gzip")
        .arg("-c")
        .arg("-6")
        .arg(src)
        .stdout(std::process::Stdio::from(f))
        .status()
        .map_err(|e| StoreError::Backup(format!("gzip: {e}")))?;
    if !status.success() {
        // Remove the partial file: a truncated .gz that sorts newest would be picked as
        // "the latest backup" and would fail only when someone tried to restore it.
        let _ = std::fs::remove_file(out);
        return Err(StoreError::Backup("gzip failed".into()));
    }
    Ok(())
}

/// Snapshots in `dest`, oldest first.
pub fn snapshots(dest: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dest) else { return vec![] };
    let mut v: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("paos-") && n.ends_with(".db.gz"))
        })
        .collect();
    // Sorted by NAME, which is a UTC timestamp — mtime would reorder them the first time
    // a sync service touched a file.
    v.sort();
    v
}

/// Keep the newest `keep`. Returns how many were removed.
pub fn prune(dest: &Path, keep: usize) -> usize {
    let snaps = snapshots(dest);
    if snaps.len() <= keep {
        return 0;
    }
    let doomed = &snaps[..snaps.len() - keep];
    for f in doomed {
        let _ = std::fs::remove_file(f);
    }
    doomed.len()
}

pub fn latest(dest: &Path) -> Option<PathBuf> {
    snapshots(dest).pop()
}

/// `paos-<UTC stamp>.db.gz`
pub fn stamped_name(now_utc: &str) -> String {
    format!("paos-{now_utc}.db.gz")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("paos-bk-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn seed_db(path: &Path, facts: usize) {
        let c = Connection::open(path).unwrap();
        paos_memory_schema(&c);
        for i in 0..facts {
            c.execute("INSERT INTO memories(id,dataset,text,embedding,created_ts) \
                       VALUES(?1,'ds',?2,x'00','t')",
                      rusqlite::params![format!("id{i}"), format!("fact {i}")]).unwrap();
        }
    }

    fn paos_memory_schema(c: &Connection) {
        c.execute_batch(
            "CREATE TABLE memories(id TEXT PRIMARY KEY, dataset TEXT, text TEXT, \
             embedding BLOB, created_ts TEXT, superseded TEXT);").unwrap();
    }

    #[test]
    fn a_snapshot_is_written_and_reports_its_contents() {
        let d = tmp("ok");
        let src = d.join("paos.db");
        seed_db(&src, 3);
        let out = d.join("paos-20260731T000000Z.db.gz");
        let msg = snapshot(&src, &out).unwrap();
        assert!(out.exists());
        assert!(msg.contains("3 facts"), "{msg}");
    }

    #[test]
    fn an_empty_store_is_refused_rather_than_rotated_in() {
        // Keeping it could push 14 real backups out of the retention window.
        let d = tmp("empty");
        let src = d.join("paos.db");
        seed_db(&src, 0);
        let out = d.join("snap.db.gz");
        assert!(snapshot(&src, &out).is_err());
        assert!(!out.exists(), "a refused snapshot must not be left on disk");
    }

    #[test]
    fn a_missing_database_is_an_error_not_an_empty_backup() {
        let d = tmp("missing");
        assert!(snapshot(&d.join("nope.db"), &d.join("o.gz")).is_err());
    }

    #[test]
    fn the_snapshot_really_is_a_readable_database() {
        // integrity_check passing inside snapshot() is not the same as the file on disk
        // being restorable; this gunzips it back and reads a row.
        let d = tmp("round");
        let src = d.join("paos.db");
        seed_db(&src, 2);
        let out = d.join("s.db.gz");
        snapshot(&src, &out).unwrap();
        let back = d.join("restored.db");
        let f = std::fs::File::create(&back).unwrap();
        assert!(std::process::Command::new("gunzip").arg("-c").arg(&out)
                .stdout(std::process::Stdio::from(f)).status().unwrap().success());
        let c = Connection::open(&back).unwrap();
        let n: i64 = c.query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn two_overlapping_snapshots_do_not_delete_each_others_workspace() {
        // The daemon's timer and a manual `paos backup run` can overlap. A per-process
        // tempdir meant each cleaned up the other mid-VACUUM.
        let d = tmp("concurrent");
        let src = d.join("paos.db");
        seed_db(&src, 1);
        let (a, b) = (d.join("a.db.gz"), d.join("b.db.gz"));
        std::thread::scope(|s| {
            let (s1, s2) = (&src, &src);
            let h1 = s.spawn(move || snapshot(s1, &a));
            let h2 = s.spawn(move || snapshot(s2, &b));
            h1.join().unwrap().expect("first snapshot");
            h2.join().unwrap().expect("second snapshot");
        });
    }

    #[test]
    fn prune_keeps_the_newest_and_removes_the_rest() {
        let d = tmp("prune");
        for i in 0..5 {
            std::fs::write(d.join(format!("paos-2026073{i}T000000Z.db.gz")), b"x").unwrap();
        }
        assert_eq!(prune(&d, 2), 3);
        let left = snapshots(&d);
        assert_eq!(left.len(), 2);
        assert!(left[1].to_string_lossy().contains("20260734"));
    }

    #[test]
    fn prune_is_a_noop_below_the_retention_count() {
        let d = tmp("noprune");
        std::fs::write(d.join("paos-1.db.gz"), b"x").unwrap();
        assert_eq!(prune(&d, 14), 0);
        assert_eq!(snapshots(&d).len(), 1);
    }

    #[test]
    fn unrelated_files_in_the_destination_are_never_pruned() {
        // The destination is a Drive folder a human may also use.
        let d = tmp("foreign");
        std::fs::write(d.join("holiday.jpg"), b"x").unwrap();
        std::fs::write(d.join("paos-1.db.gz"), b"x").unwrap();
        prune(&d, 0);
        assert!(d.join("holiday.jpg").exists());
    }

    #[test]
    fn latest_is_empty_when_nothing_has_been_backed_up() {
        assert!(latest(&tmp("none")).is_none());
    }

    #[test]
    fn a_local_destination_is_not_reported_as_offsite() {
        // The whole point of the warning: same disk, same failure.
        assert!(!is_offsite(Path::new("/Users/x/.paos-backups")));
        assert!(is_offsite(Path::new("/Users/x/Library/CloudStorage/GoogleDrive-a/My Drive/paos-backups")));
    }
}
