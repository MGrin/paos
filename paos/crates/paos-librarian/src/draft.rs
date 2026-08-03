//! Turning notes or a transcript into queued proposals.

use crate::llm;
use crate::prompts;

/// One candidate fact as the distiller returned it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Candidate {
    pub text: String,
    pub scope: Option<String>,
    pub rationale: Option<String>,
    /// `tidy` needs these. Dropping them silently produced a "merge" that recorded
    /// nothing about what it replaced.
    pub replaces: Option<Vec<String>>,
    pub why: Option<String>,
}

/// Pull a JSON array of candidates out of an LLM reply, tolerating markdown fences and
/// surrounding prose.
///
/// Deliberately lenient about the WRAPPER and strict about the CONTENT: the outermost
/// `[` to the last `]`, then only objects with non-empty text survive. A model that
/// prefixes "Here are the facts:" should not cost a whole pass.
pub fn parse_candidates(raw: &str) -> Vec<Candidate> {
    let s = raw.trim();
    let (Some(start), Some(end)) = (s.find('['), s.rfind(']')) else {
        return Vec::new();
    };
    if end < start {
        return Vec::new();
    }
    let Ok(arr) = serde_json::from_str::<serde_json::Value>(&s[start..=end]) else {
        return Vec::new();
    };
    let Some(items) = arr.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for c in items {
        if !c.is_object() {
            continue;
        }
        let text = c.get("text").and_then(|v| v.as_str()).unwrap_or("").trim();
        if text.is_empty() {
            continue;
        }
        out.push(Candidate {
            text: text.to_string(),
            scope: c.get("scope").and_then(|v| v.as_str()).map(str::to_string),
            rationale: c.get("rationale").and_then(|v| v.as_str()).map(str::to_string),
            replaces: c.get("replaces").and_then(|v| v.as_array()).map(|a| {
                a.iter().filter_map(|x| x.as_str()).map(str::to_string).collect()
            }),
            why: c.get("why").and_then(|v| v.as_str()).map(str::to_string),
        });
    }
    out
}

/// Distill notes into candidates via the configured backend.
///
/// `fallback` is the whole safety story and differs by CALLER, not by preference:
///   * hand-written `draft` passes true — if the distiller is down, keep the operator's
///     own note verbatim rather than losing what they typed.
///   * `dream` passes FALSE — its input is a machine transcript, and enqueuing a raw
///     transcript chunk as a "memory" would be worse than enqueuing nothing.
pub fn distill(
    notes: &str,
    scope_hint: Option<&str>,
    fallback: bool,
    backend: &str,
) -> Vec<Candidate> {
    distill_with(|sys, user| complete(sys, user, backend), notes, scope_hint, fallback)
}

/// `distill`, with the completion injected.
///
/// The seam exists because the alternative bit immediately: a test written against the
/// "unavailable backend" case found LM Studio actually RUNNING on this machine, took 81
/// seconds, and asserted against whatever the model happened to say. A suite that reaches
/// a live model is slow, non-deterministic, and spends the operator's subscription to
/// test control flow that has nothing to do with the model.
pub fn distill_with<F>(
    complete_fn: F,
    notes: &str,
    scope_hint: Option<&str>,
    fallback: bool,
) -> Vec<Candidate>
where
    F: FnOnce(&str, &str) -> Option<String>,
{
    let raw = complete_fn(prompts::DISTILL_SYS, notes);
    let cands = raw.as_deref().map(parse_candidates).unwrap_or_default();
    if !cands.is_empty() {
        return cands;
    }
    if !fallback {
        return Vec::new();
    }
    if notes.trim().is_empty() {
        return Vec::new();
    }
    eprintln!("[librarian] distill produced no candidates — storing the raw note verbatim");
    vec![Candidate {
        text: notes.trim().to_string(),
        scope: scope_hint.map(str::to_string),
        rationale: Some("distiller unavailable — raw note, edit on approve".to_string()),
        ..Default::default()
    }]
}

/// One completion on whichever backend is selected.
pub fn complete(system: &str, user: &str, backend: &str) -> Option<String> {
    if backend == "claude" {
        llm::claude_complete(system, user, None)
    } else {
        llm::local_chat(&llm::resolve_chat_model(&[]), system, user, None)
    }
}

/// Where a candidate belongs, and under which scope label.
///
/// `session_dataset` — the repo the SESSION was working in — is AUTHORITATIVE when known.
/// It used to be advisory, and a candidate the distiller labelled "global" bypassed it
/// entirely. The distiller only sees text and cannot know which repo produced it; in
/// practice it labelled **40 of 40** dream candidates "global", so every dreamed fact
/// landed in the global brain. Global surfaces in EVERY repo's recall, so it is the one
/// tier where being wrong is expensive.
///
/// A genuinely machine-wide fact proposed into a project queue is a small annoyance the
/// operator redirects; forty repo-specific facts in global is pollution nobody notices
/// until recall is worse everywhere.
pub fn target_dataset(
    session_dataset: Option<&str>,
    candidate_scope: Option<&str>,
    project: Option<&str>,
    org: Option<&str>,
    global: &str,
) -> (String, String) {
    if let Some(ds) = session_dataset.filter(|d| !d.is_empty()) {
        return (ds.to_string(), "project".to_string());
    }
    match candidate_scope {
        // project/org fall back to GLOBAL when they cannot resolve — e.g. run outside a
        // git repo, where there is no owner/repo to derive from.
        Some("project") => match project {
            Some(p) if !p.is_empty() => (p.to_string(), "project".to_string()),
            _ => (global.to_string(), "global".to_string()),
        },
        Some("org") => match org {
            Some(o) if !o.is_empty() => (o.to_string(), "org".to_string()),
            _ => (global.to_string(), "global".to_string()),
        },
        _ => (global.to_string(), "global".to_string()),
    }
}

/// What a candidate becomes in the queue: a fresh capture, or a supersede of a
/// near-duplicate already stored.
#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    pub kind: &'static str,
    pub dataset: String,
    pub scope: String,
    pub text: String,
    pub target_data_id: Option<String>,
    pub rationale: Option<String>,
}

/// Decide kind/dataset/rationale for one candidate.
///
/// Split from the queue write so the DECISION is testable and diffable without a
/// database — it is the part that carries the 40-of-40 scoping guard and the
/// near-duplicate branch, and both are behaviour rather than plumbing.
pub fn plan(
    cand: &Candidate,
    session_dataset: Option<&str>,
    fallback_scope: Option<&str>,
    project: Option<&str>,
    org: Option<&str>,
    global: &str,
    near_duplicate: Option<&str>,
) -> Planned {
    let cand_scope = cand.scope.as_deref().or(fallback_scope);
    let (dataset, resolved) =
        target_dataset(session_dataset, cand_scope, project, org, global);
    match near_duplicate {
        Some(dup) => Planned {
            kind: "supersede",
            dataset,
            scope: resolved,
            text: cand.text.clone(),
            target_data_id: Some(dup.to_string()),
            rationale: Some(format!(
                "{} (near-dup of {dup})",
                cand.rationale.as_deref().unwrap_or("")
            )),
        },
        None => Planned {
            kind: "capture",
            dataset,
            scope: resolved,
            text: cand.text.clone(),
            target_data_id: None,
            rationale: cand.rationale.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(text: &str, scope: Option<&str>) -> Candidate {
        Candidate {
            text: text.into(),
            scope: scope.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn a_plain_json_array_parses() {
        let c = parse_candidates(r#"[{"text":"a fact","scope":"project","rationale":"why"}]"#);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].text, "a fact");
        assert_eq!(c[0].scope.as_deref(), Some("project"));
    }

    #[test]
    fn fences_and_surrounding_prose_are_tolerated() {
        let c = parse_candidates("Here you go:\n```json\n[{\"text\":\"a fact\"}]\n```\nDone.");
        assert_eq!(c.len(), 1, "a chatty model must not cost a whole pass");
        assert_eq!(c[0].text, "a fact");
    }

    #[test]
    fn empty_and_unparseable_replies_yield_nothing() {
        assert!(parse_candidates("").is_empty());
        assert!(parse_candidates("no array here").is_empty());
        assert!(parse_candidates("[not json").is_empty());
        assert!(parse_candidates("] [").is_empty(), "end before start");
        assert!(parse_candidates(r#"[{"text":"   "}]"#).is_empty(), "blank text dropped");
        assert!(parse_candidates(r#"["a string"]"#).is_empty(), "non-objects dropped");
    }

    #[test]
    fn replaces_and_why_survive_because_tidy_needs_them() {
        let c = parse_candidates(
            r#"[{"text":"merged","replaces":["a","b"],"why":"same fact twice"}]"#);
        assert_eq!(c[0].replaces.as_deref(), Some(&["a".to_string(), "b".to_string()][..]));
        assert_eq!(c[0].why.as_deref(), Some("same fact twice"));
    }

    #[test]
    fn the_session_repo_wins_over_a_global_claim() {
        // The 40-of-40 guard. The distiller only sees text; it cannot know the repo.
        let (ds, scope) = target_dataset(
            Some("proj_acme_dotfiles"), Some("global"), Some("proj_other"),
            Some("org_x"), "global_memory");
        assert_eq!(ds, "proj_acme_dotfiles");
        assert_eq!(scope, "project");
    }

    #[test]
    fn without_a_session_repo_the_distiller_scope_applies() {
        let (ds, scope) = target_dataset(
            None, Some("org"), Some("proj_x"), Some("org_flare"), "global_memory");
        assert_eq!(ds, "org_flare");
        assert_eq!(scope, "org");
    }

    #[test]
    fn an_unresolvable_project_or_org_falls_back_to_global() {
        // Run outside a git repo there is no owner/repo to derive from.
        for claimed in ["project", "org"] {
            let (ds, scope) =
                target_dataset(None, Some(claimed), None, None, "global_memory");
            assert_eq!(ds, "global_memory");
            assert_eq!(scope, "global");
        }
    }

    #[test]
    fn an_empty_session_dataset_is_not_treated_as_known() {
        // "" must not become the dataset name; it means "not a git repo".
        let (ds, _) = target_dataset(Some(""), Some("global"), None, None, "g");
        assert_eq!(ds, "g");
    }

    #[test]
    fn a_novel_candidate_becomes_a_capture() {
        let p = plan(&cand("new fact", Some("project")), Some("proj_a"), None, None, None,
                     "g", None);
        assert_eq!(p.kind, "capture");
        assert_eq!(p.dataset, "proj_a");
        assert_eq!(p.target_data_id, None);
    }

    #[test]
    fn a_near_duplicate_becomes_a_supersede_naming_what_it_replaces() {
        let mut c = cand("restated fact", Some("project"));
        c.rationale = Some("clearer wording".into());
        let p = plan(&c, Some("proj_a"), None, None, None, "g", Some("f123"));
        assert_eq!(p.kind, "supersede");
        assert_eq!(p.target_data_id.as_deref(), Some("f123"));
        assert_eq!(p.rationale.as_deref(), Some("clearer wording (near-dup of f123)"));
    }

    #[test]
    fn a_supersede_rationale_is_still_useful_when_the_model_gave_none() {
        let p = plan(&cand("restated", None), Some("proj_a"), None, None, None, "g",
                     Some("f9"));
        assert_eq!(p.rationale.as_deref(), Some(" (near-dup of f9)"));
    }

    /// A distiller that is down. NOT a real backend: see distill_with.
    fn unavailable(_sys: &str, _user: &str) -> Option<String> {
        None
    }

    #[test]
    fn dream_never_falls_back_to_the_raw_note() {
        // fallback=false: a transcript chunk must never be enqueued as a "memory".
        let out = distill_with(unavailable, "a huge transcript chunk", None, false);
        assert!(out.is_empty(), "an unavailable distiller must enqueue NOTHING for dream");
    }

    #[test]
    fn dream_also_enqueues_nothing_when_the_model_answers_with_junk() {
        // Distinct from "down": the model replied, but with nothing parseable. Enqueuing
        // the transcript chunk here would be just as wrong.
        let out = distill_with(|_, _| Some("I could not find any durable facts.".into()),
                               "a huge transcript chunk", None, false);
        assert!(out.is_empty());
    }

    #[test]
    fn a_hand_written_note_survives_an_unavailable_distiller() {
        let out = distill_with(unavailable, "remember: the export needs a 30s timeout",
                               Some("project"), true);
        assert_eq!(out.len(), 1, "the operator's own words must not be lost");
        assert_eq!(out[0].text, "remember: the export needs a 30s timeout");
        assert_eq!(out[0].scope.as_deref(), Some("project"), "keeps the asked-for scope");
        assert!(out[0].rationale.as_deref().unwrap().contains("distiller unavailable"));
    }

    #[test]
    fn a_working_distiller_is_used_rather_than_the_fallback() {
        let out = distill_with(|_, _| Some(r#"[{"text":"a distilled fact"}]"#.into()),
                               "some notes", None, true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "a distilled fact");
    }

    #[test]
    fn the_distiller_is_handed_the_distill_prompt_and_the_notes_verbatim() {
        let mut seen = None;
        distill_with(|sys, user| { seen = Some((sys.to_string(), user.to_string())); None },
                     "MY NOTES", None, false);
        let (sys, user) = seen.expect("the completion must actually be called");
        assert_eq!(sys, prompts::DISTILL_SYS);
        assert_eq!(user, "MY NOTES", "notes go through untouched");
    }

    #[test]
    fn an_empty_note_produces_nothing_even_with_fallback() {
        assert!(distill_with(unavailable, "   ", None, true).is_empty());
    }
}
