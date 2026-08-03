//! Which repo a past SESSION was working in.
//!
//! This is the scoping seam for both `dream` and `lessons`, and it is the fix for the
//! worst mis-scoping this system has had: dream captures were landing in the global brain
//! because the only thing available was the DAEMON's cwd, and the distiller — which sees
//! only text — labelled 40 of 40 candidates "global". The session's own `cwd` is recorded
//! in the trajectory's meta record, so the right answer was on disk all along.

/// `proj_<owner>_<repo>` for a session's working directory, or `None` if it is not a git
/// repo (or git cannot answer).
///
/// Takes a runner so tests never shell out. `git` here is not a detail: the alternative —
/// deriving from the daemon's own cwd — is exactly the bug this exists to prevent.
pub fn session_dataset_with<F>(cwd: Option<&str>, git_origin: F) -> Option<String>
where
    F: FnOnce(&str) -> Option<String>,
{
    let cwd = cwd.filter(|c| !c.is_empty())?;
    let remote = git_origin(cwd)?;
    let origin = paos_memory::scope::parse_origin(&remote)?;
    Some(paos_memory::scope::project_dataset(&origin))
}

/// The production runner: `git -C <cwd> remote get-url origin`.
pub fn session_dataset(cwd: Option<&str>) -> Option<String> {
    session_dataset_with(cwd, |dir| {
        let out = std::process::Command::new("git")
            .args(["-C", dir, "remote", "get-url", "origin"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_git_repo_becomes_its_project_dataset() {
        let ds = session_dataset_with(Some("/w"), |_| {
            Some("git@github.com:acme/dotfiles.git".into())
        });
        assert_eq!(ds.as_deref(), Some("proj_acme_dotfiles"));
    }

    #[test]
    fn a_non_repo_is_none_rather_than_a_guess() {
        // None must NOT become the global brain here — the caller decides that. Returning
        // a guess is how 40 of 40 dream candidates went global.
        assert_eq!(session_dataset_with(Some("/tmp"), |_| None), None);
    }

    #[test]
    fn a_missing_or_empty_cwd_never_shells_out() {
        let mut called = false;
        let r = session_dataset_with(None, |_| {
            called = true;
            Some("x".into())
        });
        assert_eq!(r, None);
        assert!(!called, "no cwd means nothing to ask about");
        assert_eq!(session_dataset_with(Some(""), |_| Some("x".into())), None);
    }

    #[test]
    fn an_unparseable_remote_is_none() {
        assert_eq!(session_dataset_with(Some("/w"), |_| Some("not-a-url".into())), None);
    }

    #[test]
    fn the_cwd_is_passed_through_untouched() {
        let mut seen = String::new();
        session_dataset_with(Some("/a/b c"), |d| {
            seen = d.to_string();
            None
        });
        assert_eq!(seen, "/a/b c", "a path with a space must not be split");
    }
}
