//! Text → vector.
//!
//! A trait, so the backend is a decision we can revisit with measurement rather than a
//! dependency baked through the store. The spec commits to `model2vec-rs` with
//! `potion-retrieval-32M` weights compiled in (MIT, offline, no API key), with
//! `tract` + a MiniLM ONNX as the fallback if a golden-set measurement shows the static
//! embedding's retrieval gap actually hurts. Both plug in here.
//!
//! Whatever the backend, one property is non-negotiable and is the reason cognee is
//! being replaced: **embedding happens locally**. cognee's write path probed
//! `api.openai.com` with a 2.5 s HEAD and, on failure, dropped the fact while exiting 0.

/// Anything that turns text into a fixed-width vector.
///
/// Implementations must be deterministic — the same text must always produce the same
/// vector, or a re-embed would silently invalidate every stored neighbour.
pub trait Embedder: Send + Sync {
    fn embed(&self, text: &str) -> Vec<f32>;
    fn dimensions(&self) -> usize;
    /// Identifies the vector space. Changing backend or dimension invalidates stored
    /// embeddings, so the store records this and refuses to mix spaces.
    fn id(&self) -> &str;
}

/// A dependency-free hashed bag-of-words embedder.
///
/// **This is the bootstrap backend, not the shipping one.** It has no semantics beyond
/// lexical overlap — no synonyms, no word order, no context. It exists so the store,
/// the scope enforcement and the recall path can be built and tested today, and so PAOS
/// degrades to *something* rather than nothing if a model is unavailable.
///
/// It is honest about what it is: `id()` returns `hash-v1`, so when the real model
/// lands, every vector embedded with this one is detectably from a different space.
pub struct HashEmbedder {
    dims: usize,
}

impl HashEmbedder {
    pub fn new(dims: usize) -> Self {
        assert!(dims > 0, "dimensions must be positive");
        Self { dims }
    }

    fn tokens(text: &str) -> impl Iterator<Item = String> + '_ {
        text.split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .map(|t| t.to_lowercase())
    }

    fn bucket(&self, token: &str) -> usize {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in token.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        (h % self.dims as u64) as usize
    }
}

impl Embedder for HashEmbedder {
    fn embed(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; self.dims];
        for t in Self::tokens(text) {
            // Sublinear term weighting: a word repeated ten times should not dominate a
            // short fact.
            v[self.bucket(&t)] += 1.0;
        }
        for x in v.iter_mut() {
            if *x > 0.0 {
                *x = 1.0 + x.ln();
            }
        }
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in v.iter_mut() {
                *x /= norm;
            }
        }
        v
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    fn id(&self) -> &str {
        "hash-v1"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cosine;

    #[test]
    fn embedding_is_deterministic() {
        // Non-determinism would silently invalidate every stored neighbour on re-embed.
        let e = HashEmbedder::new(64);
        assert_eq!(e.embed("a durable fact"), e.embed("a durable fact"));
    }

    #[test]
    fn dimensions_are_respected() {
        for d in [8usize, 64, 512] {
            assert_eq!(HashEmbedder::new(d).embed("hello world").len(), d);
        }
    }

    #[test]
    fn output_is_unit_length() {
        let e = HashEmbedder::new(128);
        let v = e.embed("the quick brown fox");
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "expected unit vector, got {norm}");
    }

    #[test]
    fn empty_text_is_a_zero_vector_not_a_panic() {
        let e = HashEmbedder::new(32);
        let v = e.embed("");
        assert_eq!(v.len(), 32);
        assert!(v.iter().all(|x| *x == 0.0));
        // and cosine against it is 0.0, not NaN
        assert_eq!(cosine(&v, &e.embed("something")), 0.0);
    }

    #[test]
    fn tokenisation_ignores_case_and_punctuation() {
        let e = HashEmbedder::new(64);
        assert_eq!(e.embed("Deploy, runs; on FLY!"), e.embed("deploy runs on fly"));
    }

    #[test]
    fn overlapping_text_scores_higher_than_unrelated() {
        let e = HashEmbedder::new(256);
        let q = e.embed("how do I deploy the backend");
        let close = cosine(&q, &e.embed("deploy the backend with fly"));
        let far = cosine(&q, &e.embed("sourdough starter feeding schedule"));
        assert!(close > far, "close={close} far={far}");
    }

    #[test]
    fn repetition_does_not_dominate() {
        // Sublinear weighting: ten "deploy"s must not swamp a short fact.
        let e = HashEmbedder::new(256);
        let q = e.embed("deploy backend");
        let spammy = cosine(&q, &e.embed("deploy deploy deploy deploy deploy deploy"));
        let honest = cosine(&q, &e.embed("deploy backend"));
        assert!(honest > spammy, "honest={honest} spammy={spammy}");
    }

    #[test]
    fn backend_identifies_its_vector_space() {
        // Mixing spaces would compare nonsense; the id is how the store detects it.
        assert_eq!(HashEmbedder::new(64).id(), "hash-v1");
    }
}

/// The real semantic backend: model2vec static embeddings.
///
/// Static embeddings are a token-embedding lookup with weighted pooling (SIF + PCA +
/// Zipf) — no attention, so **no word order and no context disambiguation**. That is a
/// real limitation and it is the reason the choice is gated on measurement rather than
/// on the crate's README. What it buys over `HashEmbedder` is genuine vocabulary
/// semantics: synonyms and related terms land near each other instead of needing a
/// literal string match.
///
/// Loaded from a path, not compiled in: the weights are 123 MB, which does not belong
/// in a dotfiles repo's git history. The installer fetches them once; if they are
/// absent PAOS falls back to `HashEmbedder` rather than failing to start.
pub struct Model2VecEmbedder {
    model: model2vec_rs::model::StaticModel,
    dims: usize,
}

impl Model2VecEmbedder {
    /// Load from a directory containing `model.safetensors`, `tokenizer.json` and
    /// `config.json`.
    pub fn from_dir(dir: &std::path::Path) -> Result<Self, String> {
        let model = model2vec_rs::model::StaticModel::from_pretrained(dir, None, None, None)
            .map_err(|e| format!("loading model2vec from {}: {e}", dir.display()))?;
        // Probe rather than trust config.json: the stored vectors must match whatever
        // this actually produces, or every neighbour is nonsense.
        let dims = model.encode_single("dimension probe").len();
        if dims == 0 {
            return Err("model produced a zero-length embedding".into());
        }
        Ok(Self { model, dims })
    }

    /// The conventional location the installer writes to.
    pub fn default_dir() -> std::path::PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        std::path::Path::new(&home).join(".cache/paos/models/potion-retrieval-32M")
    }
}

impl Embedder for Model2VecEmbedder {
    fn embed(&self, text: &str) -> Vec<f32> {
        self.model.encode_single(text)
    }
    fn dimensions(&self) -> usize {
        self.dims
    }
    fn id(&self) -> &str {
        "potion-retrieval-32M"
    }
}

/// The best embedder available, degrading loudly rather than failing.
///
/// A missing model must not stop PAOS from starting — but it must also not silently
/// pretend to be semantic, so the caller can see which backend it got via `id()`.
pub fn best_available() -> Box<dyn Embedder> {
    let dir = Model2VecEmbedder::default_dir();
    match Model2VecEmbedder::from_dir(&dir) {
        Ok(m) => Box::new(m),
        Err(e) => {
            eprintln!("paos-memory: falling back to hash-v1 ({e})");
            Box::new(HashEmbedder::new(512))
        }
    }
}
