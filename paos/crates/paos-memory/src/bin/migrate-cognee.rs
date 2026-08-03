//! One-shot migration: cognee → the paos memory store.
//!
//! Reads cognee's **registry** (`datasets` ⋈ `dataset_data` ⋈ `data`), not the loose
//! `.txt` files. That distinction matters: there are 991 files on disk but only 981
//! registered rows, so walking the directory would import 10 orphans belonging to no
//! dataset — and scope is the entire point. Orphans are reported, never guessed at.
//!
//! Read-only with respect to cognee. It opens that database immutably, so a botched run
//! can be re-run and cognee stays intact until the new store is verified.
//!
//! Usage:
//!   migrate-cognee <cognee_db> <cognee_data_dir> [--dry-run]

use paos_memory::{best_available, ensure_schema, remember, Embedder, HashEmbedder};
use rusqlite::{Connection, OpenFlags};
use std::collections::BTreeMap;
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    if positional.len() < 2 {
        eprintln!("usage: migrate-cognee <cognee_db> <cognee_data_dir> [--dry-run]");
        std::process::exit(2);
    }
    if let Err(e) = run(positional[0], Path::new(positional[1]), dry_run) {
        eprintln!("migrate-cognee: {e}");
        std::process::exit(1);
    }
}

fn run(cognee_db: &str, data_dir: &Path, dry_run: bool) -> Result<(), Box<dyn std::error::Error>> {
    // Immutable: never mutate the source, even by accident (WAL recovery can write).
    let src = Connection::open_with_flags(
        cognee_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;

    let mut stmt = src.prepare(
        "SELECT ds.name, d.raw_data_location, d.created_at \
         FROM datasets ds \
         JOIN dataset_data dd ON dd.dataset_id = ds.id \
         JOIN data d          ON d.id = dd.data_id \
         ORDER BY ds.name",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;

    let dest_path = paos_store::db_path();
    let dest = paos_store::open(&dest_path)?;
    ensure_schema(&dest)?;
    // Dimensionality is a property of the vector space, not a tuning knob: every stored
    // embedding must come from the same one.
    // PAOS_EMBEDDER=hash forces the lexical baseline; default is the best available.
    let embedder: Box<dyn Embedder> = if std::env::var("PAOS_EMBEDDER").as_deref() == Ok("hash") {
        Box::new(HashEmbedder::new(512))
    } else {
        best_available()
    };
    let embedder = embedder.as_ref();
    paos_memory::check_space(&dest, embedder).map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    eprintln!("embedder: {}", embedder.id());

    let mut per_dataset: BTreeMap<String, usize> = BTreeMap::new();
    let (mut imported, mut missing_file, mut empty) = (0usize, 0usize, 0usize);

    for row in rows {
        let (dataset, location, created) = row?;
        let Some(loc) = location else {
            missing_file += 1;
            continue;
        };
        // cognee stores an absolute path; fall back to the data dir by basename so the
        // migration still works if the store was moved.
        let p = Path::new(&loc);
        let text = std::fs::read_to_string(p).or_else(|_| {
            let base = p.file_name().unwrap_or_default();
            std::fs::read_to_string(data_dir.join(base))
        });
        let Ok(text) = text else {
            missing_file += 1;
            continue;
        };
        if text.trim().is_empty() {
            empty += 1;
            continue;
        }
        let ts = created.unwrap_or_else(|| "1970-01-01T00:00:00Z".into());
        if !dry_run {
            remember(&dest, embedder, &dataset, text.trim(), &ts)?;
        }
        *per_dataset.entry(dataset).or_default() += 1;
        imported += 1;
    }

    // Count files on disk to surface orphans explicitly. Silence here would let 10
    // memories quietly vanish from the migration report.
    let on_disk = std::fs::read_dir(data_dir)
        .map(|it| {
            it.filter_map(Result::ok)
                .filter(|e| e.path().extension().map(|x| x == "txt").unwrap_or(false))
                .count()
        })
        .unwrap_or(0);

    println!("{}", if dry_run { "DRY RUN — nothing written" } else { "migration complete" });
    for (ds, n) in &per_dataset {
        println!("  {n:>4}  {ds}");
    }
    println!("  ----");
    println!("  {imported:>4}  imported");
    if missing_file > 0 {
        println!("  {missing_file:>4}  registry rows whose file was unreadable");
    }
    if empty > 0 {
        println!("  {empty:>4}  empty facts skipped");
    }
    let registered = imported + missing_file + empty;
    if on_disk > registered {
        println!(
            "  {:>4}  ORPHAN .txt files on disk belonging to no dataset (not imported — \
             they have no scope, and guessing one is how work memories end up in \
             personal repos)",
            on_disk - registered
        );
    }
    if !dry_run {
        println!("  -> {}", dest_path.display());
    }
    Ok(())
}
