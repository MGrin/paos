//! Scope derivation: a git origin URL → the dataset names to read and write.
//!
//! Three tiers, narrowest-that-is-still-true:
//!   * **project** `proj_<owner>_<repo>` — facts about one repo
//!   * **org**     `org_<owner>_memory`  — facts spanning one owner's repos
//!   * **global**  the global dataset    — facts true everywhere
//!
//! The names produced here must match the ones already in the store, because 981
//! migrated facts are keyed by them. Every fixture below is a real dataset name taken
//! from the live corpus rather than an invented example.

/// The global dataset name. Configurable because it is the one tier whose name is not
/// derived from anything.
/// The dataset for facts true everywhere, when configuration says nothing.
///
/// Neutral on purpose. This is COMPILED IN, so a default naming a person would file every
/// installation's global facts under that person's name.
///
/// Do not read this directly to decide where a fact goes — use `global_dataset`, which
/// honours `identity_global_dataset`. An existing machine keeps its own dataset through
/// that row, and a caller that reaches past it silently sends that machine's recall to a
/// dataset with nothing in it.
pub const DEFAULT_GLOBAL: &str = "global_memory";

/// The configured global dataset, falling back to `DEFAULT_GLOBAL`.
///
/// One function so the fallback cannot differ between callers. A read failure is a
/// fallback, not an error: a machine whose config table is unreadable must still recall
/// something rather than nothing.
pub fn global_dataset(conn: &rusqlite::Connection) -> String {
    conn.query_row(
        "SELECT value FROM paos_config WHERE key='identity_global_dataset'",
        [],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .map(|v| v.trim().to_string())
    .filter(|v| !v.is_empty())
    .unwrap_or_else(|| DEFAULT_GLOBAL.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Global,
    Org,
    Project,
}

/// Owner and repo parsed from a git remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub owner: String,
    pub repo: String,
}

/// Parse `git remote get-url origin` output.
///
/// Handles the SSH (`git@host:owner/repo.git`), HTTPS and `ssh://` forms. Returns None
/// for anything unrecognised — a wrong guess here would file facts under the wrong scope,
/// which is how work memories end up in personal repos.
pub fn parse_origin(url: &str) -> Option<Origin> {
    let u = url.trim().trim_end_matches('/');
    if u.is_empty() {
        return None;
    }
    let path = if let Some(rest) = u.strip_prefix("git@") {
        // git@github.com:owner/repo.git
        rest.split_once(':').map(|(_, p)| p)?
    } else if let Some(rest) = u.strip_prefix("ssh://") {
        let rest = rest.split_once('/').map(|(_, p)| p)?;
        rest
    } else if let Some(rest) = u.strip_prefix("https://") {
        rest.split_once('/').map(|(_, p)| p)?
    } else if let Some(rest) = u.strip_prefix("http://") {
        rest.split_once('/').map(|(_, p)| p)?
    } else {
        return None;
    };
    let path = path.trim_end_matches(".git").trim_matches('/');
    let mut parts = path.split('/').filter(|p| !p.is_empty());
    let owner = parts.next()?;
    // Take the LAST remaining segment as the repo so self-hosted forges with nested
    // groups (gitlab-style owner/group/repo) still resolve.
    let repo = parts.last()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(Origin { owner: slug(owner), repo: slug(repo) })
}

/// Lowercase, and fold anything that is not alphanumeric into `_`.
///
/// `motion-client-dashboard-ops` → `motion_client_dashboard_ops`, matching the stored
/// dataset names.
fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    out
}

pub fn project_dataset(o: &Origin) -> String {
    format!("proj_{}_{}", o.owner, o.repo)
}

pub fn org_dataset(o: &Origin) -> String {
    format!("org_{}_memory", o.owner)
}

/// Datasets to search, in tier order (project, org, global).
///
/// Order is the *priority* signal a ranker may use; it is not a filter — all three are
/// searched together and every one is scoped in SQL.
pub fn recall_scopes(origin: Option<&Origin>, global: &str) -> Vec<String> {
    match origin {
        Some(o) => vec![project_dataset(o), org_dataset(o), global.to_string()],
        // Outside a repo there is no project or org to speak of. Degrading to global is
        // correct; guessing an owner would be a leak.
        None => vec![global.to_string()],
    }
}

/// The single dataset a `remember` writes to.
pub fn write_scope(tier: Tier, origin: Option<&Origin>, global: &str) -> Result<String, String> {
    match tier {
        Tier::Global => Ok(global.to_string()),
        Tier::Org => origin
            .map(org_dataset)
            .ok_or_else(|| "no git 'origin' remote here — can't use --org; use --global".into()),
        Tier::Project => origin
            .map(project_dataset)
            .ok_or_else(|| "no git 'origin' remote here — can't use --project; use --global".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_global_dataset_names_nobody() {
        // Compiled in, so a default naming a person files every installation's global
        // facts under that person's name.
        assert_eq!(DEFAULT_GLOBAL, "global_memory");
    }

    #[test]
    fn a_configured_global_dataset_wins_over_the_default() {
        // This is what keeps an existing machine's facts reachable after the default
        // changed. Without it the row is inert and recall quietly finds nothing.
        let c = paos_store::open_in_memory().unwrap();
        assert_eq!(global_dataset(&c), DEFAULT_GLOBAL, "nothing configured yet");
        c.execute("INSERT INTO paos_config(key,value,updated_ts) \
                   VALUES('identity_global_dataset','someones_memory','t')", []).unwrap();
        assert_eq!(global_dataset(&c), "someones_memory");
    }

    #[test]
    fn an_empty_configured_value_falls_back_rather_than_naming_an_empty_dataset() {
        let c = paos_store::open_in_memory().unwrap();
        c.execute("INSERT INTO paos_config(key,value,updated_ts) \
                   VALUES('identity_global_dataset','   ','t')", []).unwrap();
        assert_eq!(global_dataset(&c), DEFAULT_GLOBAL);
    }

    fn o(owner: &str, repo: &str) -> Origin {
        Origin { owner: owner.into(), repo: repo.into() }
    }

    #[test]
    fn parses_the_real_origins_in_this_corpus() {
        // Fixtures are real remotes whose datasets hold migrated facts.
        assert_eq!(parse_origin("https://github.com/acme/dotfiles.git"), Some(o("acme", "dotfiles")));
        assert_eq!(parse_origin("git@github.com:examplecorp/motion-client-dashboard-ops.git"),
                   Some(o("examplecorp", "motion_client_dashboard_ops")));
        assert_eq!(parse_origin("https://github.com/acme/han-river-doorbell"),
                   Some(o("acme", "han_river_doorbell")));
    }

    #[test]
    fn derived_names_match_the_migrated_datasets_exactly() {
        // If these drift, 981 facts become unreachable.
        let d = parse_origin("https://github.com/acme/dotfiles.git").unwrap();
        assert_eq!(project_dataset(&d), "proj_acme_dotfiles");
        let f = parse_origin("git@github.com:examplecorp/browser-cluster.git").unwrap();
        assert_eq!(project_dataset(&f), "proj_examplecorp_browser_cluster");
        assert_eq!(org_dataset(&f), "org_examplecorp_memory");
        let m = parse_origin("git@github.com:examplecorp/motion-client-dashboard-ops.git").unwrap();
        assert_eq!(project_dataset(&m), "proj_examplecorp_motion_client_dashboard_ops");
    }

    #[test]
    fn handles_ssh_https_and_trailing_slash_forms() {
        for url in [
            "git@github.com:acme/dotfiles.git",
            "https://github.com/acme/dotfiles.git",
            "https://github.com/acme/dotfiles",
            "https://github.com/acme/dotfiles/",
            "ssh://git@github.com/acme/dotfiles.git",
        ] {
            assert_eq!(parse_origin(url), Some(o("acme", "dotfiles")), "failed for {url}");
        }
    }

    #[test]
    fn nested_groups_resolve_to_the_repo_not_the_group() {
        assert_eq!(parse_origin("https://gitlab.com/acme/team/widget.git"),
                   Some(o("acme", "widget")));
    }

    #[test]
    fn unrecognised_input_is_none_rather_than_a_guess() {
        // A wrong guess files facts under the wrong scope — the leak, by another route.
        for bad in ["", "   ", "not-a-url", "/local/path", "github.com/owner/repo"] {
            assert_eq!(parse_origin(bad), None, "should not parse: {bad:?}");
        }
    }

    #[test]
    fn recall_searches_all_three_tiers_in_order() {
        let d = o("acme", "dotfiles");
        assert_eq!(
            recall_scopes(Some(&d), DEFAULT_GLOBAL),
            vec!["proj_acme_dotfiles", "org_acme_memory", DEFAULT_GLOBAL]
        );
    }

    #[test]
    fn outside_a_repo_recall_is_global_only() {
        // Never invent an owner: that is how work facts reach personal sessions.
        assert_eq!(recall_scopes(None, DEFAULT_GLOBAL), vec![DEFAULT_GLOBAL]);
    }

    #[test]
    fn writing_project_or_org_outside_a_repo_is_an_error_not_a_fallback() {
        // Silently writing to global would put repo-specific facts in every session's
        // recall forever.
        assert!(write_scope(Tier::Project, None, DEFAULT_GLOBAL).is_err());
        assert!(write_scope(Tier::Org, None, DEFAULT_GLOBAL).is_err());
        assert_eq!(write_scope(Tier::Global, None, DEFAULT_GLOBAL).unwrap(), DEFAULT_GLOBAL);
    }

    #[test]
    fn write_scope_picks_exactly_one_dataset() {
        let d = o("acme", "dotfiles");
        assert_eq!(write_scope(Tier::Project, Some(&d), DEFAULT_GLOBAL).unwrap(), "proj_acme_dotfiles");
        assert_eq!(write_scope(Tier::Org, Some(&d), DEFAULT_GLOBAL).unwrap(), "org_acme_memory");
        assert_eq!(write_scope(Tier::Global, Some(&d), DEFAULT_GLOBAL).unwrap(), DEFAULT_GLOBAL);
    }
}
