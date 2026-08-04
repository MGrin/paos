//! A real transformer embedder, so a candidate can be measured rather than argued about.
//!
//! The shipping embedder is `potion-retrieval-32M`, a STATIC model: a 129 MB lookup table
//! with no attention, so "how hot does water get" and "the kettle boils" have nothing to
//! bring them together beyond shared tokens. Measured on this machine's own corpus, the
//! top five results for a real question land inside 0.02 of each other in the worst
//! brain — the ranking has nothing left to rank on, and no weight or blend repairs that.
//!
//! `bge-small-en-v1.5` costs almost exactly the same download (33.4M parameters versus a
//! 32M-entry table) and scores 51.68 on MTEB Retrieval against 35.06. That is the reason
//! to try it; whether it helps THIS corpus is a question for `rank-bench`, because two
//! obvious wins already evaporated under measurement.
//!
//! candle rather than fastembed/ort: `ort` links ONNX Runtime's C++ library, which would
//! cost the single-binary, no-toolchain property that the rest of this workspace protects.

use crate::embed::Embedder;
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use std::path::Path;
use tokenizers::Tokenizer;

/// Longest input the model accepts. Facts above this are truncated, not rejected.
///
/// The corpus this was written for tops out around 420 tokens, so truncation is a guard
/// rather than a routine event — but a fact that silently failed to embed would be a fact
/// that silently stopped being recalled.
const MAX_TOKENS: usize = 512;

pub struct BertEmbedder {
    model: BertModel,
    tokenizer: Tokenizer,
    dims: usize,
    id: String,
}

impl BertEmbedder {
    /// Load from a directory holding `config.json`, `tokenizer.json` and
    /// `model.safetensors` — the same three files the static model already ships as, so
    /// the download path does not change shape.
    pub fn from_dir(dir: &Path) -> Result<Self, String> {
        let cfg: Config = serde_json::from_str(
            &std::fs::read_to_string(dir.join("config.json"))
                .map_err(|e| format!("reading config.json in {}: {e}", dir.display()))?,
        )
        .map_err(|e| format!("parsing config.json in {}: {e}", dir.display()))?;

        let mut tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| format!("reading tokenizer.json in {}: {e}", dir.display()))?;
        // Truncation is configured on the tokenizer rather than by slicing ids, so the
        // [SEP] terminator is preserved; a hand-truncated sequence loses it and the model
        // sees a sentence that never ends.
        let _ = tokenizer.with_truncation(Some(tokenizers::TruncationParams {
            max_length: MAX_TOKENS,
            ..Default::default()
        }));

        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(
                &[dir.join("model.safetensors")],
                DType::F32,
                &Device::Cpu,
            )
            .map_err(|e| format!("loading model.safetensors in {}: {e}", dir.display()))?
        };
        let model = BertModel::load(vb, &cfg)
            .map_err(|e| format!("building bert from {}: {e}", dir.display()))?;

        let id = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "bert".into());
        let mut e = Self { model, tokenizer, dims: 0, id };
        // Probe rather than trust config.json, exactly as the static loader does: the
        // stored vectors must match what this actually produces or every neighbour is
        // nonsense, and hidden_size is not always the output width.
        e.dims = e.embed("dimension probe").len();
        if e.dims == 0 {
            return Err("model produced a zero-length embedding".into());
        }
        Ok(e)
    }

    /// Where the installer puts it.
    pub fn default_dir() -> std::path::PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        std::path::Path::new(&home).join(".cache/paos/models/bge-small-en-v1.5")
    }

    /// One forward pass, CLS-pooled and L2-normalised.
    ///
    /// CLS, not mean pooling. BGE is trained with the first token as the sentence
    /// representation, and mean pooling a BGE model is the classic way to get an embedder
    /// that runs, returns plausible-looking vectors, and quietly retrieves badly.
    fn forward(&self, text: &str) -> Result<Vec<f32>, candle_core::Error> {
        let enc = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| candle_core::Error::Msg(e.to_string()))?;
        let ids = Tensor::new(enc.get_ids(), &Device::Cpu)?.unsqueeze(0)?;
        let types = Tensor::zeros_like(&ids)?;
        let mask = Tensor::new(enc.get_attention_mask(), &Device::Cpu)?.unsqueeze(0)?;
        let out = self.model.forward(&ids, &types, Some(&mask))?;
        let cls = out.i((0, 0))?;
        let norm = cls.sqr()?.sum_all()?.sqrt()?.to_scalar::<f32>()?;
        // A zero vector would divide to NaN and poison every cosine it touches, so an
        // empty or all-padding input returns zeros rather than garbage.
        if norm <= f32::EPSILON {
            return Ok(vec![0.0; cls.dims1()?]);
        }
        (cls / norm as f64)?.to_vec1::<f32>()
    }
}

use candle_core::IndexOp;

impl Embedder for BertEmbedder {
    fn embed(&self, text: &str) -> Vec<f32> {
        // Infallible by contract, like every other embedder here: recall must not fail
        // because one fact tokenised oddly. A zero vector scores 0.0 against everything,
        // so the fact drops out of ranking instead of taking the process down.
        self.forward(text).unwrap_or_else(|_| vec![0.0; self.dims.max(1)])
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    fn id(&self) -> &str {
        &self.id
    }
}
