//! The models in a model folder: one sub-folder per model, named after it.
//!
//! ```text
//! model/
//!   minilm/
//!     model.toml          the manifest (see crate::manifest)
//!     config.json         the Hugging Face model config
//!     tokenizer.json      the Hugging Face tokenizer
//!     model.safetensors   the weights
//! ```
//!
//! [`Registry::scan`] reads the manifests and configs (not the weights) and checks them.
//! A folder that fails a check is skipped, with the reason in [`Registry::skipped`], and
//! the others are still used. Models are found only by their folder names, and a name
//! from a query is only looked up, never joined to a path.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use graphbuff::json::{self, Json};

use crate::manifest::{Architecture, Manifest, valid_name};

pub const MANIFEST: &str = "model.toml";
pub const CONFIG: &str = "config.json";
pub const TOKENIZER: &str = "tokenizer.json";
pub const WEIGHTS: &str = "model.safetensors";

/// Where a model's tokens get their position from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Positions {
    /// A learned table, `max_position_embeddings` rows; token `i` uses row `offset + i`
    /// (BERT: 0; XLM-RoBERTa: its `pad_token_id + 1`).
    Learned { offset: usize },
    /// Rotary embeddings (RoPE, the halves rotated) on queries and keys, with this base.
    Rotary { base: f64 },
}

/// The feed-forward block of a layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedForward {
    /// `out(gelu(in(x)))` (BERT, XLM-RoBERTa).
    Gelu,
    /// `down(up(x) * silu(gate(x)))` (nomic-bert).
    SwiGlu,
}

/// The shape of an encoder model, from its `config.json`.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelConfig {
    pub architecture: Architecture,
    pub vocab_size: usize,
    pub hidden_size: usize,
    pub num_hidden_layers: usize,
    pub num_attention_heads: usize,
    pub intermediate_size: usize,
    /// Rows of the learned position table (0 for rotary positions).
    pub max_position_embeddings: usize,
    /// The longest input in tokens the model takes.
    pub max_tokens: usize,
    pub type_vocab_size: usize,
    pub layer_norm_eps: f64,
    pub positions: Positions,
    pub feed_forward: FeedForward,
    /// Whether the query, key, value and attention output projections have biases.
    pub attention_bias: bool,
    /// Whether the feed-forward's first projections (`in`; `up` and `gate`) have biases.
    pub ffn_in_bias: bool,
    /// Whether its last projection (`out`, `down`) has one.
    pub ffn_out_bias: bool,
}

/// The `model_type` that `config.json` gives each architecture.
fn model_type(a: Architecture) -> &'static str {
    match a {
        Architecture::Bert => "bert",
        Architecture::XlmRoberta => "xlm-roberta",
        Architecture::NomicBert => "nomic_bert",
    }
}

impl ModelConfig {
    /// The `config.json` text of a model of `architecture`.
    pub fn parse(text: &str, architecture: Architecture) -> Result<ModelConfig, String> {
        let j = json::parse(text).map_err(|e| format!("bad JSON at byte {}: {}", e.offset, e.message))?;
        let Json::Object(o) = j else { return Err("not a JSON object".into()) };
        let int = |key: &str| match o.get(key) {
            Some(Json::Int(n)) if *n > 0 => Ok(*n as usize),
            Some(_) => Err(format!("`{key}` must be a positive integer")),
            None => Err(format!("`{key}` is missing")),
        };
        let float = |key: &str| match o.get(key) {
            Some(Json::Float(f)) => Some(*f),
            Some(Json::Int(i)) => Some(*i as f64),
            _ => None,
        };
        let flag = |key: &str, default: bool| match o.get(key) {
            None | Some(Json::Null) => Ok(default),
            Some(Json::Bool(b)) => Ok(*b),
            Some(_) => Err(format!("`{key}` must be true or false")),
        };
        let string = |key: &str| match o.get(key) {
            Some(Json::String(s)) => Some(s.as_str()),
            _ => None,
        };
        if let Some(t) = string("model_type")
            && t != model_type(architecture)
        {
            let fits = Architecture::ALL
                .into_iter()
                .find(|&a| model_type(a) == t)
                .map_or(String::new(), |a| format!("; set architecture = \"{}\" in model.toml", a.name()));
            return Err(format!("model_type `{t}` is not {}{fits}", architecture.name()));
        }
        let c = match architecture {
            Architecture::Bert | Architecture::XlmRoberta => {
                if let Some(act) = o.get("hidden_act")
                    && act != &Json::String("gelu".into())
                {
                    return Err(format!("hidden_act {act:?} is not supported (only gelu)"));
                }
                if let Some(p) = string("position_embedding_type")
                    && p != "absolute"
                {
                    return Err(format!("position_embedding_type `{p}` is not supported (only absolute)"));
                }
                let rows = int("max_position_embeddings")?;
                // XLM-RoBERTa numbers positions from pad_token_id + 1 (rows up to the
                // padding id are not used).
                let offset = match architecture {
                    Architecture::XlmRoberta => match o.get("pad_token_id") {
                        Some(Json::Int(p)) if *p >= 0 => *p as usize + 1,
                        None => 2,
                        Some(_) => return Err("`pad_token_id` must be a whole number".into()),
                    },
                    _ => 0,
                };
                if rows <= offset {
                    return Err(format!("max_position_embeddings {rows} leaves no positions after {offset}"));
                }
                ModelConfig {
                    architecture,
                    vocab_size: int("vocab_size")?,
                    hidden_size: int("hidden_size")?,
                    num_hidden_layers: int("num_hidden_layers")?,
                    num_attention_heads: int("num_attention_heads")?,
                    intermediate_size: int("intermediate_size")?,
                    max_position_embeddings: rows,
                    max_tokens: rows - offset,
                    type_vocab_size: int("type_vocab_size").unwrap_or(2),
                    layer_norm_eps: float("layer_norm_eps").unwrap_or(1e-12),
                    positions: Positions::Learned { offset },
                    feed_forward: FeedForward::Gelu,
                    attention_bias: true,
                    ffn_in_bias: true,
                    ffn_out_bias: true,
                }
            }
            Architecture::NomicBert => {
                if string("activation_function") != Some("swiglu") {
                    return Err("activation_function must be \"swiglu\"".into());
                }
                for key in ["prenorm", "causal", "parallel_block", "use_rms_norm", "rotary_emb_interleaved"] {
                    if flag(key, false)? {
                        return Err(format!("`{key}: true` is not supported"));
                    }
                }
                if float("rotary_emb_fraction").unwrap_or(1.0) != 1.0 {
                    return Err("only rotary_emb_fraction 1.0 is supported".into());
                }
                for key in ["rotary_scaling_factor", "rotary_emb_scale_base", "num_experts"] {
                    if o.get(key).is_some_and(|v| *v != Json::Null && *v != Json::Int(0)) {
                        return Err(format!("`{key}` is not supported"));
                    }
                }
                let positions = int("n_positions")?;
                ModelConfig {
                    architecture,
                    vocab_size: int("vocab_size")?,
                    hidden_size: int("n_embd")?,
                    num_hidden_layers: int("n_layer")?,
                    num_attention_heads: int("n_head")?,
                    // Its gated MLP rounds the width up to a multiple of 256.
                    intermediate_size: int("n_inner")?.next_multiple_of(256),
                    max_position_embeddings: 0,
                    // Without rotary scaling, inputs longer than it was trained on work
                    // but embed worse.
                    max_tokens: int("max_trained_positions").unwrap_or(positions).min(positions),
                    type_vocab_size: int("type_vocab_size").unwrap_or(2),
                    layer_norm_eps: float("layer_norm_epsilon").or(float("layer_norm_eps")).unwrap_or(1e-12),
                    positions: Positions::Rotary { base: float("rotary_emb_base").unwrap_or(10_000.0) },
                    feed_forward: FeedForward::SwiGlu,
                    attention_bias: flag("qkv_proj_bias", true)?,
                    ffn_in_bias: flag("mlp_fc1_bias", true)?,
                    ffn_out_bias: flag("mlp_fc2_bias", true)?,
                }
            }
        };
        if !c.hidden_size.is_multiple_of(c.num_attention_heads) {
            return Err(format!(
                "hidden_size {} is not a multiple of num_attention_heads {}",
                c.hidden_size, c.num_attention_heads
            ));
        }
        if matches!(c.positions, Positions::Rotary { .. }) && !(c.hidden_size / c.num_attention_heads).is_multiple_of(2) {
            return Err("rotary positions need an even head size".into());
        }
        Ok(c)
    }
}

/// A model that passed the checks.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelSpec {
    pub name: String,
    pub dir: PathBuf,
    pub manifest: Manifest,
    pub config: ModelConfig,
}

impl ModelSpec {
    /// The longest input in tokens: the manifest's `max_tokens`, else the model's limit.
    pub fn max_tokens(&self) -> usize {
        self.manifest.max_tokens.unwrap_or(self.config.max_tokens)
    }

    /// Checks one model folder.
    pub fn read(name: &str, dir: &Path) -> Result<ModelSpec, String> {
        if !valid_name(name) {
            return Err("the folder name is not a valid model name (a-z, 0-9, `.`, `_`, `-`; at most 64)".into());
        }
        let read = |file: &str| std::fs::read_to_string(dir.join(file)).map_err(|e| format!("{file}: {e}"));
        let manifest = Manifest::parse(&read(MANIFEST)?).map_err(|e| format!("{MANIFEST}: {e}"))?;
        let config =
            ModelConfig::parse(&read(CONFIG)?, manifest.architecture).map_err(|e| format!("{CONFIG}: {e}"))?;
        for file in [TOKENIZER, WEIGHTS] {
            if !dir.join(file).is_file() {
                return Err(format!("{file} is missing"));
            }
        }
        if manifest.dimensions != config.hidden_size {
            return Err(format!(
                "{MANIFEST} says {} dimensions, but the model's hidden_size is {}",
                manifest.dimensions, config.hidden_size
            ));
        }
        if let Some(m) = manifest.max_tokens
            && m > config.max_tokens
        {
            return Err(format!("max_tokens {m} is more than the model's {} tokens", config.max_tokens));
        }
        Ok(ModelSpec { name: name.to_string(), dir: dir.to_path_buf(), manifest, config })
    }
}

/// A folder that is not a usable model, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub name: String,
    pub reason: String,
}

/// The models of a model folder.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    models: BTreeMap<String, ModelSpec>,
    skipped: Vec<Skipped>,
}

impl Registry {
    /// Reads the model folder `dir`. Fails only if `dir` cannot be read; bad models are
    /// skipped (see [`Registry::skipped`]). Files directly in `dir` are ignored.
    pub fn scan(dir: &Path) -> std::io::Result<Registry> {
        let mut r = Registry::default();
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            if !e.file_type()?.is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            match ModelSpec::read(&name, &e.path()) {
                Ok(spec) => {
                    r.models.insert(name, spec);
                }
                Err(reason) => r.skipped.push(Skipped { name, reason }),
            }
        }
        Ok(r)
    }

    /// A registry of these models (for tests and embedding without a folder scan).
    pub fn from_specs(specs: impl IntoIterator<Item = ModelSpec>) -> Registry {
        Registry { models: specs.into_iter().map(|s| (s.name.clone(), s)).collect(), skipped: Vec::new() }
    }

    /// The usable models, by name.
    pub fn models(&self) -> impl Iterator<Item = &ModelSpec> {
        self.models.values()
    }

    pub fn get(&self, name: &str) -> Option<&ModelSpec> {
        self.models.get(name)
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    /// The folders that are not usable models, by name.
    pub fn skipped(&self) -> &[Skipped] {
        &self.skipped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOMIC: &str = r#"{"model_type": "nomic_bert", "activation_function": "swiglu", "vocab_size": 192, "n_embd": 32,
        "n_layer": 2, "n_head": 4, "n_inner": 64, "n_positions": 64, "max_trained_positions": 24,
        "rotary_emb_base": 1000, "rotary_emb_fraction": 1.0, "qkv_proj_bias": false, "mlp_fc1_bias": false,
        "mlp_fc2_bias": false, "rotary_scaling_factor": null}"#;

    #[test]
    fn architectures_read_their_configs() {
        let xlmr = r#"{"model_type": "xlm-roberta", "vocab_size": 81, "hidden_size": 32, "num_hidden_layers": 2,
            "num_attention_heads": 4, "intermediate_size": 64, "max_position_embeddings": 26, "pad_token_id": 1,
            "type_vocab_size": 1, "hidden_act": "gelu"}"#;
        let c = ModelConfig::parse(xlmr, Architecture::XlmRoberta).unwrap();
        assert_eq!((c.positions, c.max_tokens, c.type_vocab_size), (Positions::Learned { offset: 2 }, 24, 1));
        let c = ModelConfig::parse(NOMIC, Architecture::NomicBert).unwrap();
        assert_eq!(c.positions, Positions::Rotary { base: 1000.0 });
        assert_eq!((c.hidden_size, c.intermediate_size, c.max_tokens), (32, 256, 24), "the gated MLP's width is rounded up");
        assert!(!c.attention_bias && !c.ffn_in_bias && !c.ffn_out_bias);
        assert_eq!(c.feed_forward, FeedForward::SwiGlu);
    }

    #[test]
    fn the_architecture_must_fit_the_model_type() {
        let e = ModelConfig::parse(NOMIC, Architecture::Bert).unwrap_err();
        assert!(e.contains("model_type `nomic_bert` is not bert; set architecture = \"nomic-bert\""), "{e}");
        let e = ModelConfig::parse(r#"{"model_type": "xlm-roberta"}"#, Architecture::Bert).unwrap_err();
        assert!(e.contains("set architecture = \"xlm-roberta\""), "{e}");
        let e = ModelConfig::parse(r#"{"model_type": "qwen3"}"#, Architecture::Bert).unwrap_err();
        assert_eq!(e, "model_type `qwen3` is not bert");
    }

    #[test]
    fn unsupported_nomic_options() {
        for (from, to, err) in [
            (r#""activation_function": "swiglu""#, r#""activation_function": "gelu""#, "swiglu"),
            (r#""rotary_scaling_factor": null"#, r#""rotary_scaling_factor": 2.0"#, "rotary_scaling_factor"),
            (r#""rotary_emb_fraction": 1.0"#, r#""rotary_emb_fraction": 0.5"#, "rotary_emb_fraction"),
            (r#""qkv_proj_bias": false"#, r#""prenorm": true"#, "prenorm"),
        ] {
            let e = ModelConfig::parse(&NOMIC.replace(from, to), Architecture::NomicBert).unwrap_err();
            assert!(e.contains(err), "{to}: {e}");
        }
    }
}
