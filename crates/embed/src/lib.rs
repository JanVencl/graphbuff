//! Local text embedding models for graphbuff: a [`graphbuff::Embedder`] that runs
//! sentence encoders (BERT: MiniLM, BGE, E5, GTE …; XLM-RoBERTa: multilingual E5, bge-m3;
//! nomic-bert: nomic-embed-text) in [Burn](https://burn.dev), on the CPU or, with the
//! `gpu` feature, through wgpu.
//!
//! Models live in a folder, one sub-folder per model (see [`registry`]). They are read
//! from Hugging Face files as they are (`config.json`, `tokenizer.json`,
//! `model.safetensors`) plus a small `model.toml` ([`manifest`]). Nothing is downloaded.
//!
//! ```no_run
//! use std::sync::Arc;
//! use graphbuff::Database;
//! use graphbuff_embed::{EmbedderConfig, LocalEmbedder, Registry};
//!
//! let registry = Registry::scan("model".as_ref())?;
//! let embedder = LocalEmbedder::new(registry, EmbedderConfig::default()).map_err(std::io::Error::other)?;
//! let db = Database::in_memory();
//! db.set_embedder(Arc::new(embedder));
//! db.execute("CREATE (:Doc {text: 'hello', embedding: vector.embed('hello')})").unwrap();
//! # Ok::<(), std::io::Error>(())
//! ```

pub mod encoder;
pub mod cache;
pub mod manifest;
pub mod registry;
pub mod tokenize;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use burn::tensor::Device;
use graphbuff::{EmbedOptions, EmbedStats, EmbedTask, Embedder, Error, ModelInfo, Result, TokenSpans, accounting};

pub use manifest::{Architecture, Manifest, Pooling};
pub use registry::{FeedForward, ModelConfig, ModelSpec, Positions, Registry, Skipped};

use encoder::Encoder;
use cache::ModelCache;
use tokenize::{TextTokenizer, Tokens};

/// Where the models run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DeviceKind {
    /// The CPU, with Burn's pure-Rust `flex` backend (the default).
    #[default]
    Cpu,
    /// The GPU through wgpu; needs the `gpu` feature.
    Gpu,
}

/// Settings of a [`LocalEmbedder`] (the `[embedding]` section of the server's config).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmbedderConfig {
    /// The model used when a query names none; `None`: the only model, if there is one.
    pub default_model: Option<String>,
    /// Texts per forward pass.
    pub batch_size: usize,
    /// Threads that run the models; 0: half the cores (at least 1).
    pub threads: usize,
    /// Models kept in memory at once.
    pub max_loaded_models: usize,
    pub device: DeviceKind,
}

impl Default for EmbedderConfig {
    fn default() -> Self {
        EmbedderConfig { default_model: None, batch_size: 32, threads: 0, max_loaded_models: 2, device: DeviceKind::Cpu }
    }
}

/// A model in memory.
pub struct LoadedModel {
    tokenizer: TextTokenizer,
    encoder: Encoder,
}

/// A model that was just loaded, for [`LocalEmbedder::on_load`].
#[derive(Clone, Debug)]
pub struct LoadInfo<'a> {
    pub name: &'a str,
    pub dimensions: usize,
    /// The size of its weights file.
    pub bytes: u64,
    pub took: Duration,
}

/// Called with every model loaded.
type OnLoad = Box<dyn Fn(&LoadInfo<'_>) + Send + Sync>;

#[derive(Default)]
struct Counters {
    calls: AtomicU64,
    texts: AtomicU64,
    tokens: AtomicU64,
    nanos: AtomicU64,
    load_nanos: AtomicU64,
}

/// Runs the models of a [`Registry`]. See the [crate docs](crate).
pub struct LocalEmbedder {
    registry: Registry,
    config: EmbedderConfig,
    default_model: Option<String>,
    device: Device,
    adapter: Option<String>,
    pool: rayon::ThreadPool,
    cache: ModelCache<LoadedModel>,
    counters: Counters,
    on_load: Option<OnLoad>,
}

impl std::fmt::Debug for LocalEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalEmbedder")
            .field("models", &self.registry.models().map(|m| &m.name).collect::<Vec<_>>())
            .field("default_model", &self.default_model)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// The device of `kind`, and for the GPU its adapter (name and graphics API).
fn device(kind: DeviceKind) -> std::result::Result<(Device, Option<String>), String> {
    match kind {
        DeviceKind::Cpu => Ok((Device::from(burn::backend::FlexDevice), None)),
        #[cfg(feature = "gpu")]
        DeviceKind::Gpu => gpu_device().map(|(d, a)| (d, Some(a))),
        #[cfg(not(feature = "gpu"))]
        DeviceKind::Gpu => Err("the GPU needs a build with the `gpu` feature".into()),
    }
}

/// The wgpu device, brought up once per process so that its adapter is known (and a
/// machine without one fails here, not in the first query).
#[cfg(feature = "gpu")]
fn gpu_device() -> std::result::Result<(Device, String), String> {
    static GPU: std::sync::OnceLock<std::result::Result<(Device, String), String>> = std::sync::OnceLock::new();
    GPU.get_or_init(|| {
        let (device, setup) =
            Device::wgpu_options().init_with_setup().map_err(|e| format!("no GPU for the embedding models: {e}"))?;
        let info = setup.adapter.get_info();
        Ok((device, format!("{} ({:?})", info.name, info.backend)))
    })
    .clone()
}

impl LocalEmbedder {
    /// An embedder of `registry`'s models. Fails if `config.default_model` is not one of
    /// them, or the device is not available.
    pub fn new(registry: Registry, config: EmbedderConfig) -> std::result::Result<LocalEmbedder, String> {
        let default_model = match &config.default_model {
            Some(name) if registry.get(name).is_none() => {
                return Err(format!("the default embedding model `{name}` is not installed"));
            }
            Some(name) => Some(name.clone()),
            None => {
                let mut models = registry.models();
                match (models.next(), models.next()) {
                    (Some(only), None) => Some(only.name.clone()),
                    _ => None,
                }
            }
        };
        let threads = match config.threads {
            0 => std::thread::available_parallelism().map_or(1, |n| (n.get() / 2).max(1)),
            n => n,
        };
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("graphbuff-embed-{i}"))
            .build()
            .map_err(|e| format!("starting the embedding threads: {e}"))?;
        let (device, adapter) = device(config.device)?;
        Ok(LocalEmbedder {
            device,
            adapter,
            cache: ModelCache::new(config.max_loaded_models),
            registry,
            config,
            default_model,
            pool,
            counters: Counters::default(),
            on_load: None,
        })
    }

    /// Calls `f` after each model load (to log it). Weights are not query memory: they
    /// are shared and stay loaded after the query that loaded them.
    pub fn on_load(mut self, f: impl Fn(&LoadInfo<'_>) + Send + Sync + 'static) -> Self {
        self.on_load = Some(Box::new(f));
        self
    }

    /// Runs `f` on the embedding threads. A panic in it (a backend that fails inside, for
    /// one) becomes an error, so it fails the query instead of the thread that called.
    fn on_pool<T>(&self, f: impl FnOnce() -> std::result::Result<T, String> + Send) -> std::result::Result<T, String>
    where
        T: Send,
    {
        guarded(|| self.pool.install(f))
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// The GPU adapter the models run on (name and graphics API); `None` on the CPU.
    pub fn adapter(&self) -> Option<&str> {
        self.adapter.as_deref()
    }

    /// The model used when a query names none.
    pub fn default_model(&self) -> Option<&str> {
        self.default_model.as_deref()
    }

    /// How many times a model was loaded (models stay loaded between calls).
    pub fn loads(&self) -> usize {
        self.cache.loads()
    }

    fn spec(&self, model: Option<&str>) -> Result<&ModelSpec> {
        let name = match model.or(self.default_model.as_deref()) {
            Some(n) => n,
            None => {
                return Err(Error::Semantic(
                    "no embedding model named and no default: name one, or set `embedding.default_model`".into(),
                ));
            }
        };
        self.registry.get(name).ok_or_else(|| {
            let known: Vec<&str> = self.registry.models().map(|m| m.name.as_str()).collect();
            Error::Semantic(format!(
                "unknown embedding model '{name}' (installed: {})",
                if known.is_empty() { "none".to_string() } else { known.join(", ") }
            ))
        })
    }

    /// The model `spec`, loading it if needed.
    fn model(&self, spec: &ModelSpec) -> Result<Arc<LoadedModel>> {
        self.cache
            .get(&spec.name, || {
                let started = Instant::now();
                let weights = spec.dir.join(registry::WEIGHTS);
                let model = accounting::exempt(|| -> std::result::Result<LoadedModel, String> {
                    let tokenizer = TextTokenizer::load(&spec.dir.join(registry::TOKENIZER), spec.max_tokens())?;
                    let encoder = self.on_pool(|| Encoder::load(&weights, &spec.config, &self.device))?;
                    Ok(LoadedModel { tokenizer, encoder })
                })?;
                let took = started.elapsed();
                self.counters.load_nanos.fetch_add(took.as_nanos() as u64, Ordering::Relaxed);
                if let Some(f) = &self.on_load {
                    let bytes = std::fs::metadata(&weights).map_or(0, |m| m.len());
                    f(&LoadInfo { name: &spec.name, dimensions: spec.manifest.dimensions, bytes, took });
                }
                Ok(model)
            })
            .map_err(|e| Error::Runtime(format!("loading embedding model '{}': {e}", spec.name)))
    }

    /// The token ids `model` gives `text` (special tokens included), as the model sees
    /// them: with the prefix of `options.task`, cut to `max_tokens`.
    pub fn token_ids(&self, model: Option<&str>, text: &str, options: &EmbedOptions) -> Result<Vec<u32>> {
        let spec = self.spec(model)?;
        let m = self.model(spec)?;
        let text = with_prefix(spec, text, options.task);
        let mut t = m.tokenizer.encode(&[&text], options.truncate).map_err(Error::Semantic)?;
        Ok(t.pop().map(|t| t.ids).unwrap_or_default())
    }
}

/// `f`, with a panic turned into an error that carries its message.
fn guarded<T>(f: impl FnOnce() -> std::result::Result<T, String>) -> std::result::Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|p| {
        let msg = p
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "no message".into());
        Err(format!("the model backend failed: {msg}"))
    })
}

fn prefix(spec: &ModelSpec, task: EmbedTask) -> &str {
    match task {
        EmbedTask::Query => &spec.manifest.query_prefix,
        EmbedTask::Document => &spec.manifest.doc_prefix,
    }
}

fn with_prefix(spec: &ModelSpec, text: &str, task: EmbedTask) -> String {
    format!("{}{text}", prefix(spec, task))
}

impl Embedder for LocalEmbedder {
    fn default_model(&self) -> Option<String> {
        self.default_model.clone()
    }

    fn models(&self) -> Vec<ModelInfo> {
        self.registry
            .models()
            .map(|m| ModelInfo {
                name: m.name.clone(),
                dimensions: m.manifest.dimensions,
                max_tokens: m.max_tokens(),
                loaded: self.cache.is_loaded(&m.name),
            })
            .collect()
    }

    fn stats(&self) -> EmbedStats {
        let c = &self.counters;
        EmbedStats {
            calls: c.calls.load(Ordering::Relaxed),
            texts: c.texts.load(Ordering::Relaxed),
            tokens: c.tokens.load(Ordering::Relaxed),
            seconds: c.nanos.load(Ordering::Relaxed) as f64 / 1e9,
            model_loads: self.cache.loads() as u64,
            load_seconds: c.load_nanos.load(Ordering::Relaxed) as f64 / 1e9,
        }
    }

    fn token_spans(&self, model: Option<&str>, text: &str, task: EmbedTask) -> Result<TokenSpans> {
        let spec = self.spec(model)?;
        let m = self.model(spec)?;
        let (prefix_tokens, spans) = self.on_pool(|| m.tokenizer.spans(prefix(spec, task), text)).map_err(Error::Semantic)?;
        Ok(TokenSpans { spans, fit: m.tokenizer.text_tokens().saturating_sub(prefix_tokens) })
    }

    fn embed(
        &self,
        model: Option<&str>,
        texts: &[&str],
        options: &EmbedOptions,
        interrupt: &dyn Fn() -> Result<()>,
    ) -> Result<Vec<Vec<f32>>> {
        let started = Instant::now();
        let spec = self.spec(model)?;
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        interrupt()?;
        let m = self.model(spec)?;
        let texts: Vec<String> = texts.iter().map(|t| with_prefix(spec, t, options.task)).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let tokens: Vec<Tokens> =
            self.on_pool(|| m.tokenizer.encode(&refs, options.truncate)).map_err(Error::Semantic)?;
        // Batches of similar lengths pad less: sort by length, embed, put back in order.
        let mut order: Vec<usize> = (0..tokens.len()).collect();
        order.sort_by_key(|&i| tokens[i].ids.len());
        let mut out: Vec<Vec<f32>> = vec![Vec::new(); tokens.len()];
        for chunk in order.chunks(self.config.batch_size.max(1)) {
            let padded = m.tokenizer.pad(&chunk.iter().map(|&i| &tokens[i]).collect::<Vec<_>>());
            // The pass runs on the pool's threads, whose memory the query's thread does
            // not count: charge its estimate to the query while it runs (and check it).
            let _charge = accounting::charge(m.encoder.activation_bytes(padded.rows, padded.width));
            interrupt()?;
            let vectors = self
                .on_pool(|| m.encoder.embed(&padded, spec.manifest.pooling, spec.manifest.normalize))
                .map_err(|e| Error::Runtime(format!("embedding model '{}': {e}", spec.name)))?;
            for (&i, v) in chunk.iter().zip(vectors) {
                out[i] = v;
            }
        }
        let c = &self.counters;
        c.calls.fetch_add(1, Ordering::Relaxed);
        c.texts.fetch_add(tokens.len() as u64, Ordering::Relaxed);
        c.tokens.fetch_add(tokens.iter().map(|t| t.ids.len() as u64).sum(), Ordering::Relaxed);
        c.nanos.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::guarded;

    #[test]
    fn a_panic_becomes_an_error() {
        assert_eq!(guarded(|| Ok::<_, String>(1)), Ok(1));
        let e = guarded::<()>(|| panic!("kernel {} broke", 7)).unwrap_err();
        assert_eq!(e, "the model backend failed: kernel 7 broke");
        let e = guarded::<()>(|| panic!("static")).unwrap_err();
        assert!(e.ends_with("static"));
    }
}
