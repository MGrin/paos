//! Run the lessons funnel over real transcripts and print it, for a diff against Python.
//!
//! Prints, in order:
//!   `episodes\t<n>` · `signatures\t<n>` · `recurring\t<n>`
//!   then one `recur\t<sessions>\t<signature>` line per surviving signature
//!   then one `scope\t<signature>\t<dataset|->` line
//!
//! Counterpart to `paos/parity/lessons_parity.py`. Both walk the SAME snapshot directory
//! so a difference is the funnel and not the input.

use paos_librarian::lessons::{self, Episode};
use paos_librarian::session::session_dataset;
use paos_trajectory as tj;

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: lessons-parity <projects-dir> [limit]");
        std::process::exit(2);
    });
    let limit: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(40);

    let listing = match tj::list_trajectories(
        "claude-code",
        limit,
        None,
        None,
        Some(std::path::Path::new(&root)),
        0.0,
    ) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("lessons-parity: {e}");
            std::process::exit(1);
        }
    };

    // FIRST-ENCOUNTER order: the tie-break in `recurring` depends on it.
    let mut groups = lessons::Groups::new();
    let mut n_episodes = 0usize;
    for item in &listing.items {
        let Ok(bytes) = std::fs::read(&item.path) else { continue };
        let raw = String::from_utf8_lossy(&bytes);
        let Ok(norm) = tj::normalize_transcript("claude-code", &raw, tj::DEFAULT_TRUNCATE)
        else {
            continue;
        };
        if norm.records.is_empty() {
            continue;
        }
        let (sid, cwd) = match &norm.records[0] {
            tj::Record::Meta(m) => (
                m.session_id.clone().unwrap_or_else(|| item.path.clone()),
                m.cwd.clone(),
            ),
            _ => (item.path.clone(), None),
        };
        let ds = session_dataset(cwd.as_deref());
        for ep in tj::failure_episodes(&norm.records, 6) {
            n_episodes += 1;
            let g = groups.entry(&ep.signature);
            g.episodes.push(Episode {
                tool: ep.tool,
                args: ep.args,
                error: ep.error,
                signature: ep.signature,
                recovery: ep.recovery,
            });
            g.sessions.insert(sid.clone());
            g.datasets.push(ds.clone());
        }
    }

    if std::env::var("DUMP_ALL").is_ok() {
        let mut all = String::new();
        for (sig, g) in &groups.ordered() {
            all.push_str(&format!("{}\t{}\n", g.sessions.len(),
                                  sig.replace('\n', "\\n").replace('\t', "\\t")));
        }
        print!("{all}");
        return;
    }
    let ordered = groups.ordered();
    let recur = lessons::recurring(&ordered, lessons::MIN_SESSIONS, tj::is_teachable);
    let mut out = String::new();
    out.push_str(&format!("episodes\t{n_episodes}\n"));
    out.push_str(&format!("signatures\t{}\n", groups.len()));
    // `recurring` is already capped; report the uncapped count too, as Python does.
    let uncapped = ordered
        .iter()
        .filter(|(s, g)| g.sessions.len() >= lessons::MIN_SESSIONS && tj::is_teachable(s))
        .count();
    out.push_str(&format!("recurring\t{uncapped}\n"));
    for (sig, g) in &recur {
        out.push_str(&format!("recur\t{}\t{}\n", g.sessions.len(), sig));
        out.push_str(&format!(
            "scope\t{}\t{}\n",
            sig,
            lessons::scope_dataset(&g.datasets).unwrap_or_else(|| "-".into())
        ));
    }
    print!("{out}");
}
