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

use crate::manifest::{Manifest, valid_name};

pub const MANIFEST: &str = "model.toml";
pub const CONFIG: &str = "config.json";
pub const TOKENIZER: &str = "tokenizer.json";
pub const WEIGHTS: &str = "model.safetensors";

/// The shape of a BERT model, from its `config.json`.
#[derive(Clone, Debug, PartialEq)]
pub struct BertConfig {
    pub vocab_size: usize,
    pub hidden_size: usize,
    pub num_hidden_layers: usize,
    pub num_attention_heads: usize,
    pub intermediate_size: usize,
    pub max_position_embeddings: usize,
    pub type_vocab_size: usize,
    pub layer_norm_eps: f64,
}

impl BertConfig {
    pub fn parse(text: &str) -> Result<BertConfig, String> {
        let j = json::parse(text).map_err(|e| format!("bad JSON at byte {}: {}", e.offset, e.message))?;
        let Json::Object(o) = j else { return Err("not a JSON object".into()) };
        let int = |key: &str| match o.get(key) {
            Some(Json::Int(n)) if *n > 0 => Ok(*n as usize),
            Some(_) => Err(format!("`{key}` must be a positive integer")),
            None => Err(format!("`{key}` is missing")),
        };
        if let Some(act) = o.get("hidden_act")
            && act != &Json::String("gelu".into())
        {
            return Err(format!("hidden_act {act:?} is not supported (only gelu)"));
        }
        if let Some(Json::String(p)) = o.get("position_embedding_type")
            && p != "absolute"
        {
            return Err(format!("position_embedding_type `{p}` is not supported (only absolute)"));
        }
        let c = BertConfig {
            vocab_size: int("vocab_size")?,
            hidden_size: int("hidden_size")?,
            num_hidden_layers: int("num_hidden_layers")?,
            num_attention_heads: int("num_attention_heads")?,
            intermediate_size: int("intermediate_size")?,
            max_position_embeddings: int("max_position_embeddings")?,
            type_vocab_size: int("type_vocab_size").unwrap_or(2),
            layer_norm_eps: match o.get("layer_norm_eps") {
                Some(Json::Float(f)) => *f,
                Some(Json::Int(i)) => *i as f64,
                _ => 1e-12,
            },
        };
        if !c.hidden_size.is_multiple_of(c.num_attention_heads) {
            return Err(format!(
                "hidden_size {} is not a multiple of num_attention_heads {}",
                c.hidden_size, c.num_attention_heads
            ));
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
    pub config: BertConfig,
}

impl ModelSpec {
    /// The longest input in tokens: the manifest's `max_tokens`, else the model's limit.
    pub fn max_tokens(&self) -> usize {
        self.manifest.max_tokens.unwrap_or(self.config.max_position_embeddings)
    }

    /// Checks one model folder.
    pub fn read(name: &str, dir: &Path) -> Result<ModelSpec, String> {
        if !valid_name(name) {
            return Err("the folder name is not a valid model name (a-z, 0-9, `.`, `_`, `-`; at most 64)".into());
        }
        let read = |file: &str| std::fs::read_to_string(dir.join(file)).map_err(|e| format!("{file}: {e}"));
        let manifest = Manifest::parse(&read(MANIFEST)?).map_err(|e| format!("{MANIFEST}: {e}"))?;
        let config = BertConfig::parse(&read(CONFIG)?).map_err(|e| format!("{CONFIG}: {e}"))?;
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
            && m > config.max_position_embeddings
        {
            return Err(format!(
                "max_tokens {m} is more than the model's max_position_embeddings {}",
                config.max_position_embeddings
            ));
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
