//! Installing the embedding model.
//!
//! `best_available()` degrades to `hash-v1` when the model is absent, with one line on
//! stderr — so a machine that never ran this has measurably worse recall and almost no
//! signal about why. That was invisible for as long as only one machine ran paos.

use std::path::Path;

/// Pinned to `main` of a model that has not changed since release. A revision hash would
/// be stricter, but a moving `main` on a released model is not the risk here — and a
/// wrong hash fails as a 404 nobody can diagnose.
const REPO: &str = "https://huggingface.co/minishlab/potion-retrieval-32M/resolve/main";

/// The second-stage judge. Same three files, so it reuses the whole installer.
///
/// A separate 133 MB and therefore a separate, OPTIONAL install: it doubles the download
/// and buys about 18% MRR, which is a trade the person installing gets to make. Recall
/// works without it, exactly as it did before.
const RERANK_REPO: &str =
    "https://huggingface.co/BAAI/bge-small-en-v1.5/resolve/main";

#[derive(Debug, PartialEq, Eq)]
pub enum Install {
    AlreadyPresent,
    Installed,
    Failed(String),
}

/// All three, or the directory is not a model. `from_pretrained` needs every one, so a
/// partial fetch produces a directory that looks installed and fails at load.
pub fn files() -> [&'static str; 3] {
    ["config.json", "model.safetensors", "tokenizer.json"]
}

pub fn url(file: &str) -> String {
    format!("{REPO}/{file}")
}

pub fn rerank_url(file: &str) -> String {
    format!("{RERANK_REPO}/{file}")
}

/// Install the second-stage model. Same shape as `ensure`, different repo.
pub fn ensure_rerank(dir: &Path) -> Install {
    ensure_from(dir, &rerank_url, &fetch)
}

/// Is every file present AND non-empty?
///
/// Non-empty matters: curl leaves a zero-byte file behind when it dies mid-flight, and
/// "the file exists" would call that installed.
fn complete(dir: &Path) -> bool {
    files()
        .iter()
        .all(|f| std::fs::metadata(dir.join(f)).map(|m| m.len() > 0).unwrap_or(false))
}

pub fn ensure(dir: &Path) -> Install {
    ensure_with(dir, &fetch)
}

/// The fetcher is a parameter so every branch is testable without a network.
pub fn ensure_with(dir: &Path, fetch: &dyn Fn(&str, &Path) -> Result<(), String>) -> Install {
    ensure_from(dir, &url, fetch)
}

/// The shared installer: which repo is a parameter, so the second model cannot drift into
/// a second copy of the resume/rename/verify logic that this one already gets right.
pub fn ensure_from(
    dir: &Path,
    url_of: &dyn Fn(&str) -> String,
    fetch: &dyn Fn(&str, &Path) -> Result<(), String>,
) -> Install {
    if complete(dir) {
        return Install::AlreadyPresent;
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        return Install::Failed(format!("creating {}: {e}", dir.display()));
    }
    for f in files() {
        let target = dir.join(f);
        if std::fs::metadata(&target).map(|m| m.len() > 0).unwrap_or(false) {
            continue;
        }
        // Download beside the target and rename, so an interrupted run never leaves a
        // truncated file that the next run counts as present.
        let tmp = dir.join(format!("{f}.part"));
        if let Err(e) = fetch(&url_of(f), &tmp) {
            let _ = std::fs::remove_file(&tmp);
            return Install::Failed(format!("{f}: {e}"));
        }
        if let Err(e) = std::fs::rename(&tmp, &target) {
            return Install::Failed(format!("{f}: {e}"));
        }
    }
    if complete(dir) { Install::Installed } else { Install::Failed("incomplete".into()) }
}

fn fetch(url: &str, target: &Path) -> Result<(), String> {
    let out = std::process::Command::new("curl")
        .args(["-sSL", "--fail", "--max-time", "600", "-o"])
        .arg(target)
        .arg(url)
        .output()
        .map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_files_a_static_model_needs() {
        // model2vec's from_pretrained wants all three in one directory. Fetching two of
        // them leaves a directory that looks installed and fails at load.
        assert_eq!(files(), ["config.json", "model.safetensors", "tokenizer.json"]);
    }

    #[test]
    fn the_url_points_at_the_pinned_repository() {
        assert_eq!(url("config.json"),
            "https://huggingface.co/minishlab/potion-retrieval-32M/resolve/main/config.json");
    }

    #[test]
    fn a_complete_directory_is_already_present_and_is_not_refetched() {
        // 129 MB. Re-running the wizard must not re-download it.
        let dir = std::env::temp_dir().join(format!("paos-model-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in files() {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        assert_eq!(ensure(&dir), Install::AlreadyPresent);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_partial_directory_is_not_already_present() {
        // The failure this catches: an interrupted download leaves config.json behind,
        // and treating that as installed gives a machine that loads nothing and says
        // nothing.
        let dir = std::env::temp_dir().join(format!("paos-model-p-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), "x").unwrap();
        assert_ne!(ensure_with(&dir, &|_, _| Err("no network".into())),
                   Install::AlreadyPresent);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_file_does_not_count_as_downloaded() {
        // curl leaves a zero-byte file behind when it fails mid-flight.
        let dir = std::env::temp_dir().join(format!("paos-model-e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in files() {
            std::fs::write(dir.join(f), "").unwrap();
        }
        assert_ne!(ensure_with(&dir, &|_, _| Err("no network".into())),
                   Install::AlreadyPresent);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failed_download_leaves_no_partial_file_behind() {
        // The next run must not count a half-file as present, and .part must not linger
        // as litter either.
        let dir = std::env::temp_dir().join(format!("paos-model-f-{}", std::process::id()));
        let r = ensure_with(&dir, &|_, p| {
            std::fs::write(p, "half").unwrap();
            Err("connection reset".into())
        });
        assert!(matches!(r, Install::Failed(_)), "{r:?}");
        assert!(!dir.join("config.json").exists(), "no target was created");
        assert!(!dir.join("config.json.part").exists(), "the partial was cleaned up");
        std::fs::remove_dir_all(&dir).ok();
    }
}
