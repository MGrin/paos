//! Ranking quality against a REAL store: does the right fact come back FIRST?
//!
//! `semantic-bench` answers a different question — which *embedder* wins — and needs two
//! purpose-built stores embedded with different models. Its golden set is hardcoded
//! against placeholder scope names, so it cannot be pointed at a store that actually
//! exists. This one takes the golden set as a file, so it measures the store you have.
//!
//! That distinction matters because ranking is tuned by a handful of knobs
//! (`PAOS_LEXICAL_WEIGHT`, the usefulness decay) and a knob without a measurement is a
//! guess. `--sweep` exists so a proposed weight is compared against the current one on
//! the same queries rather than argued about.
//!
//! Golden set format — TSV, one case per line, `#` comments and blank lines ignored:
//!
//! ```text
//! <dataset>\t<question in an agent's own words>\t<substring identifying the right fact>
//! ```
//!
//! Deliberately NOT JSON: this crate has no serde, and a bench tool is not worth a
//! dependency. Write questions the way an agent would ask them, avoiding the target
//! fact's distinctive vocabulary — the reported lexical overlap keeps that honest. A
//! high overlap means the question leaked the answer's words and the case proves little.
//!
//! Usage: `rank-bench <paos.db> <golden.tsv> [--top-k N] [--sweep|--sweep-alias]`

use paos_memory::{recall, Embedder, Model2VecEmbedder};
#[cfg(feature = "bert")]
use paos_memory::BertEmbedder;
use rusqlite::Connection;
use std::collections::HashSet;

#[derive(Debug)]
struct Case {
    dataset: String,
    question: String,
    needle: String,
}

fn parse_golden(raw: &str) -> Result<Vec<Case>, String> {
    let mut out = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        let line = line.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        // A silently-skipped malformed line would shrink the golden set without shrinking
        // the reported denominator, which is the one failure that makes a bench lie.
        if cols.len() < 3 {
            return Err(format!(
                "line {}: expected 3 tab-separated columns, got {}",
                i + 1,
                cols.len()
            ));
        }
        let (dataset, question, needle) = (cols[0].trim(), cols[1].trim(), cols[2].trim());
        if dataset.is_empty() || question.is_empty() || needle.is_empty() {
            return Err(format!("line {}: empty column", i + 1));
        }
        out.push(Case {
            dataset: dataset.to_string(),
            question: question.to_string(),
            needle: needle.to_string(),
        });
    }
    Ok(out)
}

fn lexical_overlap(q: &str, target: &str) -> f64 {
    let t: HashSet<String> = target.split_whitespace().map(|w| w.to_lowercase()).collect();
    let qw: Vec<String> = q.split_whitespace().map(|w| w.to_lowercase()).collect();
    if qw.is_empty() {
        return 0.0;
    }
    qw.iter().filter(|w| t.contains(*w)).count() as f64 / qw.len() as f64
}

struct Report {
    hit1: usize,
    hitk: usize,
    mrr: f64,
    overlap: f64,
    misses: Vec<String>,
}

/// Re-order the first-stage candidates with a second, slower model.
///
/// Measured motivation: splitting the worst brain into ~23-fact pools and routing each
/// question perfectly took it from hit@5 3/8 to 8/8. The right fact is nearly always
/// present and badly ordered, so the win is in ordering a shortlist — and a shortlist is
/// something a system can actually produce, where an oracle that picks the right
/// sub-brain is not.
#[cfg(feature = "bert")]
fn rerank(e: &dyn Embedder, query: &str, texts: &[String]) -> Vec<usize> {
    fn cos(a: &[f32], b: &[f32]) -> f32 {
        let (mut d, mut x, mut y) = (0.0f32, 0.0f32, 0.0f32);
        for (p, q) in a.iter().zip(b) {
            d += p * q;
            x += p * p;
            y += q * q;
        }
        if x <= 0.0 || y <= 0.0 { 0.0 } else { d / (x.sqrt() * y.sqrt()) }
    }
    let q = e.embed(query);
    let mut scored: Vec<(usize, f32)> = texts
        .iter()
        .enumerate()
        .map(|(i, t)| (i, cos(&q, &e.embed(t))))
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.into_iter().map(|(i, _)| i).collect()
}

/// Rank of the correct fact for each case, 0-indexed; `None` is a miss.
fn ranks(conn: &Connection, e: &dyn Embedder, cases: &[Case], top_k: usize) -> Vec<Option<usize>> {
    cases
        .iter()
        .map(|c| {
            // BGE is trained with an instruction on the QUERY side only. Its card says
            // v1.5 degrades only slightly without it, but "slightly" is a claim about
            // MTEB, not about this corpus — so it is a flag, and measured either way.
            let q = match std::env::var("PAOS_QUERY_PREFIX") {
                Ok(p) if !p.is_empty() => format!("{p}{}", c.question),
                _ => c.question.clone(),
            };
            let hits = recall(conn, e, &[c.dataset.clone()], &q, top_k).unwrap_or_default();
            let needle = c.needle.to_lowercase();
            hits.iter()
                .position(|h| h.memory.text.to_lowercase().contains(&needle))
        })
        .collect()
}

fn score(conn: &Connection, e: &dyn Embedder, cases: &[Case], top_k: usize) -> Report {
    let mut r = Report { hit1: 0, hitk: 0, mrr: 0.0, overlap: 0.0, misses: Vec::new() };
    for (c, pos) in cases.iter().zip(ranks(conn, e, cases, top_k)) {
        match pos {
            Some(p) => {
                if p == 0 {
                    r.hit1 += 1;
                }
                r.hitk += 1;
                r.mrr += 1.0 / (p + 1) as f64;
                // Overlap is measured against the fact we RETRIEVED at that rank, which is
                // the one the needle matched — the same text a reader would judge.
                let hits =
                    recall(conn, e, &[c.dataset.clone()], &c.question, top_k).unwrap_or_default();
                r.overlap += lexical_overlap(&c.question, &hits[p].memory.text);
            }
            None => r.misses.push(c.question.clone()),
        }
    }
    let n = cases.len().max(1) as f64;
    r.mrr /= n;
    if r.hitk > 0 {
        r.overlap /= r.hitk as f64;
    }
    r
}

fn detail(conn: &Connection, e: &dyn Embedder, cases: &[Case], top_k: usize) {
    for (c, pos) in cases.iter().zip(ranks(conn, e, cases, top_k)) {
        let mark = match pos {
            Some(0) => "1st".to_string(),
            Some(p) => format!("{}th", p + 1),
            None => "MISS".to_string(),
        };
        println!("    {mark:>5}  [{}] {}", c.dataset, c.question);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    if positional.len() < 2 {
        eprintln!("usage: rank-bench <paos.db> <golden.tsv> [--top-k N] [--sweep]");
        std::process::exit(2);
    }
    let top_k = args
        .iter()
        .position(|a| a == "--top-k")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(5usize);
    let sweep = args.iter().any(|a| a == "--sweep");
    let sweep_alias = args.iter().any(|a| a == "--sweep-alias");
    let corpus = args.iter().any(|a| a == "--corpus");

    let raw = match std::fs::read_to_string(positional[1]) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("rank-bench: cannot read {}: {e}", positional[1]);
            std::process::exit(2);
        }
    };
    let cases = match parse_golden(&raw) {
        Ok(c) if !c.is_empty() => c,
        Ok(_) => {
            eprintln!("rank-bench: golden set is empty");
            std::process::exit(2);
        }
        Err(e) => {
            eprintln!("rank-bench: {e}");
            std::process::exit(2);
        }
    };

    // READ-ONLY, and not merely as good hygiene: `recall` reinforces what it returns, so a
    // read-write bench teaches the store that its own questions were useful and every later
    // run scores a store the previous run edited. Measured before this was fixed — a plain
    // run reported MRR 0.414 where the sweep put the same weight at 0.486, because the
    // first pass had already re-ranked the corpus for the second.
    let conn = match Connection::open_with_flags(
        positional[0],
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rank-bench: cannot open {}: {e}", positional[0]);
            std::process::exit(2);
        }
    };
    // The hash fallback would silently measure a DIFFERENT retrieval path than the one
    // every session actually uses, and report it as the ranking — so an unavailable model
    // is a hard exit, not a downgrade.
    #[cfg(feature = "bert")]
    let bert_requested = args.iter().any(|a| a == "--bert");
    #[cfg(not(feature = "bert"))]
    let bert_requested = false;
    if !cfg!(feature = "bert") && args.iter().any(|a| a == "--bert") {
        eprintln!("rank-bench: built without the bert feature — rebuild with --features bert");
        std::process::exit(2);
    }
    let embedder: Box<dyn Embedder> = if bert_requested {
        #[cfg(feature = "bert")]
        {
            match BertEmbedder::from_dir(&BertEmbedder::default_dir()) {
                Ok(b) => Box::new(b) as Box<dyn Embedder>,
                Err(e) => {
                    eprintln!("rank-bench: bert unavailable ({e})");
                    std::process::exit(1);
                }
            }
        }
        #[cfg(not(feature = "bert"))]
        unreachable!()
    } else {
        match Model2VecEmbedder::from_dir(&Model2VecEmbedder::default_dir()) {
            Ok(m) => Box::new(m),
            Err(e) => {
                eprintln!("rank-bench: model unavailable ({e}) — refusing to report ranking");
                std::process::exit(1);
            }
        }
    };
    let embedder = embedder.as_ref();
    // A store embedded with a different model is a different coordinate system; scoring it
    // produces numbers that look fine and mean nothing.
    if let Err(e) = paos_memory::check_space(&conn, embedder) {
        eprintln!("rank-bench: {e}");
        std::process::exit(1);
    }

    let n = cases.len();
    println!("  {n} case(s), top-{top_k}, model {}", embedder.id());

    #[cfg(feature = "bert")]
    if let Some(i) = args.iter().position(|a| a == "--rerank") {
        let depth: usize = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(30);
        let second = match BertEmbedder::from_dir(&BertEmbedder::default_dir()) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("rank-bench: bert unavailable ({e})");
                std::process::exit(1);
            }
        };
        let (h1, hk, mrr) = score_reranked(&conn, embedder, &second, &cases, top_k, depth);
        println!("\n  rerank top-{depth} with {}", Embedder::id(&second));
        println!("  hit@1  {h1:>2}/{n}");
        println!("  hit@{top_k}  {hk:>2}/{n}");
        println!("  MRR    {mrr:.3}");
        return;
    }

    if corpus {
        // Diagnosis, not scoring: how much room the embedding has to tell one fact from
        // another in each brain. A high mean pairwise cosine means the corpus is flat and
        // no ranking change can help it.
        let mut brains: Vec<&str> = cases.iter().map(|c| c.dataset.as_str()).collect();
        brains.sort_unstable();
        brains.dedup();
        println!("\n  brain                              facts  pairwise  top1   margin");
        for b in brains {
            let (pw, n) = paos_memory::corpus_spread(&conn, b, 400).unwrap_or((0.0, 0));
            // The margin is the number that decides whether ranking is a decision or a
            // coin flip: how far the best hit stands above the fifth. Fact-to-fact
            // similarity says how alike the corpus is IN GENERAL; this says how much
            // room the embedder has to separate candidates FOR AN ACTUAL QUESTION, which
            // is the thing recall depends on.
            let qs: Vec<&Case> = cases.iter().filter(|c| c.dataset == b).collect();
            let (mut top1, mut margin, mut seen) = (0.0f64, 0.0f64, 0usize);
            for c in &qs {
                let hits = recall(&conn, embedder, &[b.to_string()], &c.question, 5)
                    .unwrap_or_default();
                if hits.len() >= 5 {
                    top1 += hits[0].score as f64;
                    margin += (hits[0].score - hits[4].score) as f64;
                    seen += 1;
                }
            }
            let d = seen.max(1) as f64;
            println!("  {b:<34} {n:>5}     {pw:.3}  {:.3}   {:.3}", top1 / d, margin / d);
        }
        println!("\n  pairwise = how alike the facts are to each other.");
        println!("  margin   = score@1 minus score@5 for the real questions. A small margin");
        println!("             means the top five are indistinguishable and rank order is noise.");
        return;
    }

    if sweep_alias {
        println!("\n  penalty  hit@1  hit@{top_k}    MRR");
        for step in 0..=8 {
            let w = step as f32 / 40.0;
            std::env::set_var("PAOS_ALIAS_PENALTY", format!("{w}"));
            let r = score(&conn, embedder, &cases, top_k);
            println!("  {w:>7.3}  {:>2}/{n}  {:>2}/{n}  {:>5.3}", r.hit1, r.hitk, r.mrr);
        }
        std::env::remove_var("PAOS_ALIAS_PENALTY");
        return;
    }

    if sweep {
        // The current default is included in the swept range, so the table shows whether a
        // change is an improvement over what is deployed rather than over nothing.
        println!("\n  weight  hit@1  hit@{top_k}    MRR");
        for step in 0..=10 {
            let w = step as f32 / 10.0;
            std::env::set_var("PAOS_LEXICAL_WEIGHT", format!("{w}"));
            let r = score(&conn, embedder, &cases, top_k);
            println!(
                "  {w:>6.1}  {:>2}/{n}  {:>2}/{n}  {:>5.3}",
                r.hit1, r.hitk, r.mrr
            );
        }
        std::env::remove_var("PAOS_LEXICAL_WEIGHT");
        return;
    }

    detail(&conn, embedder, &cases, top_k);
    // Per-brain, because the whole-set number hides the comparison that matters. A change
    // applied to ONE dataset moves the total by a fraction of its real effect, and reads
    // as noise next to the brains it never touched.
    let mut brains: Vec<&str> = cases.iter().map(|c| c.dataset.as_str()).collect();
    brains.sort_unstable();
    brains.dedup();
    if brains.len() > 1 {
        println!("\n  ── by brain ─────────────────────────────");
        for b in brains {
            let subset: Vec<Case> = cases
                .iter()
                .filter(|c| c.dataset == b)
                .map(|c| Case {
                    dataset: c.dataset.clone(),
                    question: c.question.clone(),
                    needle: c.needle.clone(),
                })
                .collect();
            let n = subset.len();
            let r = score(&conn, embedder, &subset, top_k);
            println!("  {:<34} hit@1 {:>2}/{n}  hit@{top_k} {:>2}/{n}  MRR {:.3}",
                     b, r.hit1, r.hitk, r.mrr);
        }
    }
    let r = score(&conn, embedder, &cases, top_k);
    println!("\n  ── results ──────────────────────────────");
    println!("  hit@1  {:>2}/{n}", r.hit1);
    println!("  hit@{top_k}  {:>2}/{n}", r.hitk);
    println!("  MRR    {:.3}", r.mrr);
    println!("  mean lexical overlap on hits: {:.0}%", r.overlap * 100.0);
    println!("  (low overlap + a hit = the match was semantic, not the question leaking the answer)");
    if !r.misses.is_empty() {
        println!("\n  misses:");
        for m in &r.misses {
            println!("    · {m}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_blank_lines_are_not_cases() {
        let g = parse_golden("# a note\n\nds\tquestion\tneedle\n\n").unwrap();
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].dataset, "ds");
        assert_eq!(g[0].needle, "needle");
    }

    #[test]
    fn a_question_containing_a_tab_would_shift_every_column() {
        // Only the first three columns are read, so a trailing note is allowed — but a
        // question that itself contains a tab must not silently become the needle.
        let g = parse_golden("ds\twhere do creds live\tkeychain\ta trailing note").unwrap();
        assert_eq!(g[0].question, "where do creds live");
        assert_eq!(g[0].needle, "keychain");
    }

    #[test]
    fn a_malformed_line_fails_loudly_rather_than_shrinking_the_set() {
        // Skipping it would drop the case AND the denominator, so the bench would report a
        // better score for having fewer questions to answer.
        let err = parse_golden("ds\tonly two columns").unwrap_err();
        assert!(err.contains("line 1"), "{err}");
    }

    #[test]
    fn an_empty_column_is_malformed() {
        assert!(parse_golden("ds\t\tneedle").is_err());
    }

    #[test]
    fn overlap_is_the_share_of_the_questions_words_present_in_the_fact() {
        assert_eq!(lexical_overlap("alpha beta", "alpha gamma"), 0.5);
        assert_eq!(lexical_overlap("", "anything"), 0.0);
    }
}

#[cfg(feature = "bert")]
/// Two-stage: `first` proposes `depth` candidates, `second` re-orders them, and only the
/// top `top_k` of that reordering count. Reported separately from `score` because the two
/// stages have different costs and conflating them hides which one is failing.
fn score_reranked(
    conn: &Connection,
    first: &dyn Embedder,
    second: &dyn Embedder,
    cases: &[Case],
    top_k: usize,
    depth: usize,
) -> (usize, usize, f64) {
    let (mut hit1, mut hitk, mut mrr) = (0usize, 0usize, 0.0f64);
    for c in cases {
        let hits = recall(conn, first, &[c.dataset.clone()], &c.question, depth).unwrap_or_default();
        let texts: Vec<String> = hits.iter().map(|h| h.memory.text.clone()).collect();
        let order = rerank(second, &c.question, &texts);
        let needle = c.needle.to_lowercase();
        if let Some(p) = order
            .iter()
            .take(top_k)
            .position(|&i| texts[i].to_lowercase().contains(&needle))
        {
            if p == 0 {
                hit1 += 1;
            }
            hitk += 1;
            mrr += 1.0 / (p + 1) as f64;
        }
    }
    (hit1, hitk, mrr / cases.len().max(1) as f64)
}
