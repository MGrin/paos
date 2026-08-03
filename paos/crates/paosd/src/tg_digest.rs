//! The digest, made ACTIONABLE.
//!
//! The digest has always been able to say "15 memory proposal(s) pending" and "⚠ DEAF:
//! vivid-cobra-2" — and then nothing. Every one of those lines ended at "review at
//! http://127.0.0.1:8788", which is a laptop, which is precisely the thing the operator
//! does not have when he is reading this. Reported 2026-08-02: "I can see this on
//! telegram … but I have no way to act on these points on telegram".
//!
//! So each line that names work gets a button that does that work. The rule this module
//! follows: a button either completes the action or opens the thing that does — it never
//! just re-states the count.
//!
//! Approving here APPLIES the proposal, through the same plan-then-execute path the CLI
//! uses (`paos_librarian::apply`). It deliberately does NOT reimplement the ordering:
//! a split stores its pieces before retiring the original, and if the store fails the
//! proposal stays pending with the original intact. Getting that wrong loses memory.

use paos_librarian::queue;
use rusqlite::Connection;

/// How many DEAF sessions get their own button before the rest become a count.
const MAX_DEAF_BUTTONS: usize = 3;

/// Inline keyboard for the digest — `None` when nothing in it is actionable.
///
/// `with_nav` adds the panel's own row, so a digest opened FROM the panel can still get
/// back to accounts/fleet/tasks. Without it a tap would strand the operator on a view
/// with no way out but typing a command.
pub fn markup(conn: &Connection, with_nav: bool, nav_row: &str) -> Option<String> {
    let mut rows: Vec<String> = Vec::new();

    let pending = pending_count(conn);
    if pending > 0 {
        rows.push(format!(
            "[{{\"text\":\"📥 review {pending} memory proposal(s)\",\"callback_data\":\"dg:props\"}}]"
        ));
    }

    let deaf = deaf_sessions(conn);
    if !deaf.is_empty() {
        // Reap, not "wake". A wake message is DELIVERED BY THE LISTENER, and a deaf
        // session is by definition the one with no listener — so a wake button would be
        // the most reassuring possible no-op. What is actually decidable from a phone is
        // "these are gone, clear them", and the reaper only takes sessions whose process
        // is confirmed dead, so a heads-down-but-alive session survives the tap.
        rows.push(format!(
            "[{{\"text\":\"🧹 clear {} dead session(s)\",\"callback_data\":\"dg:reap\"}}]",
            deaf.len()
        ));
    }

    let parked = paos_operator::open_parked(conn).unwrap_or_default();
    if !parked.is_empty() {
        rows.push(format!(
            "[{{\"text\":\"🅿 {} parked decision(s)\",\"callback_data\":\"dg:parked\"}}]",
            parked.len()
        ));
    }

    if rows.is_empty() {
        return None;
    }
    if with_nav {
        rows.push(nav_row.to_string());
    }
    Some(format!("{{\"inline_keyboard\":[{}]}}", rows.join(",")))
}

/// One pending proposal, rendered as a card with its own decision buttons.
///
/// One at a time on purpose. Fifteen proposals as fifteen cards is a scroll, not a
/// review — and the queue exists because these decisions deserve reading.
///
/// `after` is the SKIP CURSOR: show the first proposal past this id, wrapping to the
/// start. Deliberately carried in the callback data rather than written down, because
/// "not now" is not a status — a skipped proposal has to come back, and the ones he keeps
/// deferring are usually the ones that need a laptop. The first version implemented skip
/// by renumbering the row's primary key to MAX+1, which is a data mutation to represent a
/// UI position, and would have broken any spooled `proposal_set_status` still naming the
/// old id.
pub fn proposal_card(conn: &Connection, after: Option<i64>) -> (String, Option<String>) {
    let pending = queue::list_pending(conn).unwrap_or_default();
    let p = match after {
        Some(cursor) => pending.iter().find(|p| p.id > cursor).or_else(|| pending.first()),
        None => pending.first(),
    };
    let Some(p) = p else {
        return ("✓ no memory proposals pending".to_string(), None);
    };
    let mut body = format!("🗂 proposal #{} · {} ({} left)\n", p.id, p.kind, pending.len());
    if let Some(scope) = p.scope.as_deref().filter(|s| !s.trim().is_empty()) {
        body.push_str(&format!("scope: {scope}\n"));
    }
    if let Some(why) = p.rationale.as_deref().filter(|s| !s.trim().is_empty()) {
        body.push_str(&format!("why: {}\n", clamp(why, 200)));
    }
    body.push('\n');
    // The TEXT is the decision. Everything else is framing, so the text gets the room.
    body.push_str(&clamp(p.text.as_deref().unwrap_or("(no text)"), 1200));
    if let Some(t) = p.target_data_id.as_deref().filter(|s| !s.trim().is_empty()) {
        body.push_str(&format!("\n\nreplaces: {}", clamp(t, 120)));
    }
    let markup = format!(
        "{{\"inline_keyboard\":[[\
           {{\"text\":\"✅ approve\",\"callback_data\":\"mp:ok:{id}\"}},\
           {{\"text\":\"✖ reject\",\"callback_data\":\"mp:no:{id}\"}}],[\
           {{\"text\":\"⏭ next\",\"callback_data\":\"mp:skip:{id}\"}}]]}}",
        id = p.id
    );
    (body, Some(markup))
}

/// Approve — meaning APPLY, then mark it approved. Returns what to tell the operator.
///
/// The order is the whole point and is not negotiable: plan, execute every step, and only
/// then resolve. Marking it approved first would leave an unapplied "approved" proposal
/// that nothing ever retries, which is the failure mode that made the old `curate`
/// worthless — five of six approved facts never reached the store.
pub fn approve(conn: &Connection, embedder: &dyn paos_memory::Embedder, id: i64) -> String {
    let Ok(Some(p)) = queue::get(conn, id) else {
        return format!("#{id}: no such proposal");
    };
    let alive = |fid: &str| queue::fact_exists(conn, fid);
    let steps = match paos_librarian::apply::plan_apply(&p, alive) {
        Ok(s) => s,
        Err(e) => {
            if paos_librarian::apply::refusal_retires(&e) {
                // Every source it would replace is already gone. Applying it would
                // RESURRECT deleted content, so this is a rejection, not an error.
                let _ = queue::set_status(conn, id, "rejected", &crate::handlers::now_iso());
                return format!("#{id}: retired — every fact it replaces is already gone");
            }
            return format!("#{id}: cannot apply ({e:?})");
        }
    };
    let now = crate::handlers::now_iso();
    for step in &steps {
        let outcome = match step {
            paos_librarian::apply::Step::Store { dataset, text } => {
                paos_memory::remember(conn, embedder, dataset, text.trim(), &now).map(|_| ())
            }
            paos_librarian::apply::Step::StoreAndRetire { dataset, text, old_ids } => {
                match paos_memory::remember(conn, embedder, dataset, text.trim(), &now) {
                    // Store FIRST, retire second. A failed store must not cost the
                    // original, so there is nothing to undo on this path.
                    Ok(new_id) => {
                        let mut r = Ok(());
                        for old in old_ids {
                            if let Err(e) = paos_memory::supersede(conn, old, &new_id) {
                                r = Err(e);
                                break;
                            }
                        }
                        r
                    }
                    Err(e) => Err(e),
                }
            }
        };
        if let Err(e) = outcome {
            // STAYS PENDING, so the tap can simply be repeated. A partially applied
            // split is retryable precisely because the original is still there.
            return format!("#{id}: write failed, still pending — {e}");
        }
    }
    match queue::set_status(conn, id, "approved", &now) {
        Ok(true) => format!("✅ #{id} approved and applied"),
        Ok(false) => format!("#{id} was already resolved"),
        Err(e) => format!("#{id}: applied, but recording it failed — {e}"),
    }
}

pub fn reject(conn: &Connection, id: i64) -> String {
    match queue::set_status(conn, id, "rejected", &crate::handlers::now_iso()) {
        Ok(true) => format!("✖ #{id} rejected"),
        Ok(false) => format!("#{id} was already resolved"),
        Err(e) => format!("#{id}: rejecting failed — {e}"),
    }
}

/// Archive sessions whose process is confirmed dead.
pub fn reap(conn: &Connection, now_epoch: i64) -> String {
    match paos_presence::reap_dead(conn, now_epoch) {
        Ok(names) if names.is_empty() =>
            "nothing to clear — every session here still has a live process".to_string(),
        Ok(names) => format!("🧹 cleared {}: {}", names.len(), names.join(", ")),
        Err(e) => format!("clearing failed: {e}"),
    }
}

/// Parked decisions, in full — the digest only ever showed the count.
pub fn parked(conn: &Connection) -> String {
    let rows = paos_operator::open_parked(conn).unwrap_or_default();
    if rows.is_empty() {
        return "no parked decisions".to_string();
    }
    let mut out = format!("🅿 {} parked\n", rows.len());
    for (id, session, note) in &rows {
        out.push_str(&format!("• #{id} [{session}] {}\n", clamp(note, 160)));
    }
    out
}

fn pending_count(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM memory_proposals WHERE status='pending'", [], |r| r.get(0))
        .unwrap_or(0)
}

fn deaf_sessions(conn: &Connection) -> Vec<String> {
    let Ok(mut st) = conn.prepare(
        "SELECT name FROM sessions WHERE ended_ts IS NULL AND deaf_since IS NOT NULL")
    else {
        return Vec::new();
    };
    st.query_map([], |r| r.get::<_, String>(0))
        .map(|it| it.filter_map(Result::ok).take(MAX_DEAF_BUTTONS.max(1) * 10).collect())
        .unwrap_or_default()
}

/// Cut to `max` CHARACTERS — chars, not bytes, because a byte slice through an em-dash
/// panics rather than truncating, and these strings are full of them.
fn clamp(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max.saturating_sub(1)).collect::<String>().trim_end().to_string() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let c = paos_store::open_in_memory().unwrap();
        // The memory tables live in paos-memory's own schema, not the store's — approve
        // WRITES, so a test without them exercises the failure path by accident.
        paos_memory::ensure_schema(&c).unwrap();
        c
    }

    fn queue_one(c: &Connection, text: &str) -> i64 {
        queue::add(c, "split", "global_memory", Some(text), Some("global"),
                   None, Some("because"), Some("dream"), "T0").unwrap()
    }

    #[test]
    fn a_quiet_digest_has_no_buttons() {
        assert_eq!(markup(&db(), false, ""), None, "no work means no keyboard");
    }

    #[test]
    fn a_pending_proposal_gets_a_review_button() {
        let c = db();
        queue_one(&c, "a fact worth keeping");
        let m = markup(&c, false, "").expect("something is pending");
        assert!(m.contains("dg:props"), "{m}");
        assert!(m.contains("review 1"), "the count is on the button: {m}");
    }

    #[test]
    fn the_card_shows_the_text_because_the_text_is_the_decision() {
        let c = db();
        let id = queue_one(&c, "the operator prefers absolute dates in memories");
        let (body, markup) = proposal_card(&c, None);
        assert!(body.contains("absolute dates"), "{body}");
        assert!(body.contains(&format!("#{id}")), "{body}");
        let m = markup.expect("a pending proposal is decidable");
        assert!(m.contains(&format!("mp:ok:{id}")) && m.contains(&format!("mp:no:{id}")), "{m}");
    }

    #[test]
    fn an_empty_queue_says_so_and_offers_nothing_to_tap() {
        let (body, markup) = proposal_card(&db(), None);
        assert!(body.contains("no memory proposals"), "{body}");
        assert!(markup.is_none(), "nothing pending means nothing to decide");
    }

    #[test]
    fn rejecting_resolves_it_once_and_only_once() {
        let c = db();
        let id = queue_one(&c, "something wrong");
        assert!(reject(&c, id).contains("rejected"));
        assert!(reject(&c, id).contains("already resolved"), "a double tap is a no-op");
        assert_eq!(pending_count(&c), 0);
    }

    #[test]
    fn approving_writes_the_fact_and_only_then_resolves_it() {
        // The tap has to APPLY, not just mark. Marking first is how the old queue ended
        // up with approved proposals that never reached the store.
        let c = db();
        let id = queue::add(&c, "capture", "global_memory",
                            Some("mgrin prefers absolute dates in memories"), Some("global"),
                            None, None, Some("dream"), "T0").unwrap();
        let out = approve(&c, &paos_memory::HashEmbedder::new(64), id);
        assert!(out.contains("approved and applied"), "{out}");
        assert_eq!(pending_count(&c), 0);
        let stored: i64 = c
            .query_row("SELECT COUNT(*) FROM memories WHERE text LIKE '%absolute dates%'",
                       [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, 1, "the fact itself must be in the store, not just a status");
    }

    #[test]
    fn a_proposal_that_cannot_be_applied_stays_pending() {
        // The invariant that makes a repeated tap safe: nothing is resolved that was not
        // applied, so the operator can simply press it again.
        let c = db();
        let id = queue::add(&c, "supersede", "global_memory", Some("   "),
                            Some("global"), None, None, Some("dream"), "T0").unwrap();
        let out = approve(&c, &paos_memory::HashEmbedder::new(64), id);
        assert!(out.contains("cannot apply"), "{out}");
        assert_eq!(pending_count(&c), 1, "an unapplied proposal must not be resolved");
    }

    #[test]
    fn skip_moves_past_the_current_one_and_wraps_without_touching_the_row() {
        let c = db();
        let a = queue_one(&c, "first fact");
        let b = queue_one(&c, "second fact");
        let (body, _) = proposal_card(&c, Some(a));
        assert!(body.contains("second fact"), "skip advances: {body}");
        let (body, _) = proposal_card(&c, Some(b));
        assert!(body.contains("first fact"), "past the end it wraps: {body}");
        assert_eq!(pending_count(&c), 2, "skipping resolves nothing");
        assert!(queue::get(&c, a).unwrap().is_some(), "and renumbers nothing");
    }

    #[test]
    fn clamp_counts_characters_not_bytes() {
        assert_eq!(clamp("a—b—c", 3), "a—…");
        assert_eq!(clamp("short", 90), "short");
    }
}
