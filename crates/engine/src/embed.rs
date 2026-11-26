//! Text embeddings: `vector.embed(text, model?, options?)`, `CALL vector.embedBatch(texts,
//! model?, options?)` and `CALL vector.models()`.
//!
//! The engine does not run models itself. An [`Embedder`] does, set with
//! [`crate::Database::set_embedder`] or [`crate::DatabaseManager::set_embedder`], much
//! like the kernel backend. Without one, the functions and procedures fail with
//! `Unsupported`. [`HashEmbedder`] is a model-free embedder for tests.
//!
//! The result of an embedding is a plain `LIST<FLOAT>`. It can be stored as a property and
//! indexed by a vector index whose `vector.dimensions` equal the model's.
//!
//! ```
//! use std::sync::Arc;
//! use graphbuff::{Database, HashEmbedder};
//!
//! let db = Database::in_memory();
//! db.set_embedder(Arc::new(HashEmbedder::new([("test", 8)])));
//! let r = db.execute("RETURN size(vector.embed('hello world')) AS dims")?;
//! assert_eq!(r.get(0, "dims").and_then(|v| v.as_int()), Some(8));
//! # Ok::<(), graphbuff::Error>(())
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use graphbuff_core::{Error, Result, Value};

use crate::procedures::{ProcedureContext, ProcedureRows};

/// An installed embedding model, as `vector.models()` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    pub name: String,
    /// The length of the vectors it returns.
    pub dimensions: usize,
    /// The longest input it takes, in tokens.
    pub max_tokens: usize,
    /// Whether it is loaded in memory now.
    pub loaded: bool,
}

/// What the text embedded is: some models put a different prefix before queries and
/// documents (E5's `query: ` and `passage: `, for example).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EmbedTask {
    /// A document to store and search (the default).
    #[default]
    Document,
    /// A search query.
    Query,
}

/// Per-call options: the third argument of `vector.embed` and `vector.embedBatch`
/// (`{task: 'query' | 'document', truncate: true | false}`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmbedOptions {
    pub task: EmbedTask,
    /// Whether text longer than the model's `max_tokens` is cut (`true`, the default)
    /// or fails.
    pub truncate: bool,
}

impl Default for EmbedOptions {
    fn default() -> Self {
        EmbedOptions { task: EmbedTask::Document, truncate: true }
    }
}

impl EmbedOptions {
    /// The options a query passed (`null` for the defaults); `what` names the caller in errors.
    pub(crate) fn from_value(what: &str, v: &Value) -> Result<Self> {
        let mut o = EmbedOptions::default();
        let map = match v {
            Value::Null => return Ok(o),
            Value::Map(m) => m,
            other => return Err(Error::Type(format!("{what}: options must be a map, not {}", other.type_name()))),
        };
        for (k, v) in map {
            match (k.as_str(), v) {
                ("task", Value::String(t)) => {
                    o.task = match t.to_ascii_lowercase().as_str() {
                        "document" | "doc" => EmbedTask::Document,
                        "query" => EmbedTask::Query,
                        _ => return Err(Error::Semantic(format!("{what}: `task` is 'query' or 'document', not '{t}'"))),
                    }
                }
                ("truncate", Value::Bool(b)) => o.truncate = *b,
                ("task" | "truncate", Value::Null) => {}
                ("task" | "truncate", other) => {
                    return Err(Error::Type(format!("{what}: `{k}` does not accept {}", other.type_name())));
                }
                _ => return Err(Error::Semantic(format!("{what}: unknown option `{k}` (known: task, truncate)"))),
            }
        }
        Ok(o)
    }
}

/// What an embedder has done since it started, for metrics. All totals.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EmbedStats {
    /// Calls of [`Embedder::embed`] that succeeded.
    pub calls: u64,
    pub texts: u64,
    /// Tokens embedded (padding not counted).
    pub tokens: u64,
    /// Time spent in successful calls.
    pub seconds: f64,
    pub model_loads: u64,
    pub load_seconds: f64,
}

/// Turns text into vectors. Set on a database with [`crate::Database::set_embedder`].
pub trait Embedder: Send + Sync {
    /// The installed models.
    fn models(&self) -> Vec<ModelInfo>;

    /// The model [`embed`](Self::embed) uses when it is given none: the first one, unless
    /// the embedder says otherwise.
    fn default_model(&self) -> Option<String> {
        self.models().into_iter().next().map(|m| m.name)
    }

    /// Its totals so far; all zero if it keeps none.
    fn stats(&self) -> EmbedStats {
        EmbedStats::default()
    }

    /// Embeds `texts` with `model` (`None`: the default model), one vector per text and
    /// in order. Long batches call `interrupt` between parts, on the calling thread, and
    /// stop with its error. An unknown model fails with `Semantic`.
    fn embed(
        &self,
        model: Option<&str>,
        texts: &[&str],
        options: &EmbedOptions,
        interrupt: &dyn Fn() -> Result<()>,
    ) -> Result<Vec<Vec<f32>>>;
}

/// The error when no embedder is set.
pub(crate) fn not_available() -> Error {
    Error::Unsupported("embedding is not available (graphbuff was built without `embed`, or no models are installed)".into())
}

/// Embeds `texts` and checks that the embedder returned one vector per text.
fn embed_checked(
    embedder: &dyn Embedder,
    model: Option<&str>,
    texts: &[&str],
    options: &EmbedOptions,
    interrupt: &dyn Fn() -> Result<()>,
) -> Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let out = embedder.embed(model, texts, options, interrupt)?;
    if out.len() != texts.len() {
        return Err(Error::Runtime(format!("the embedder returned {} vectors for {} texts", out.len(), texts.len())));
    }
    Ok(out)
}

fn vector_value(v: Vec<f32>) -> Value {
    Value::List(v.into_iter().map(|x| Value::Float(x as f64)).collect())
}

/// The model argument: a name, or `null` for the default.
fn model_arg<'v>(what: &str, v: Option<&'v Value>) -> Result<Option<&'v str>> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(other) => Err(Error::Type(format!("{what}: the model must be a string, not {}", other.type_name()))),
    }
}

/// `vector.embed(text, model?, options?)`: `args` as the query passed them.
pub(crate) fn embed_function(
    embedder: Option<&Arc<dyn Embedder>>,
    args: &[Value],
    interrupt: &dyn Fn() -> Result<()>,
) -> Result<Value> {
    const NAME: &str = "vector.embed()";
    if args.is_empty() || args.len() > 3 {
        return Err(Error::Semantic(format!("vector.embed() expects 1 to 3 argument(s), got {}", args.len())));
    }
    let model = model_arg(NAME, args.get(1))?;
    let options = EmbedOptions::from_value(NAME, args.get(2).unwrap_or(&Value::Null))?;
    let text = match &args[0] {
        Value::Null => return Ok(Value::Null),
        Value::String(s) => s.as_str(),
        other => return Err(Error::Type(format!("{NAME} does not accept {}", other.type_name()))),
    };
    let embedder = embedder.ok_or_else(not_available)?;
    let mut out = embed_checked(embedder.as_ref(), model, &[text], &options, interrupt)?;
    Ok(vector_value(out.pop().expect("one vector per text")))
}

/// `CALL vector.embedBatch(texts, model, options) YIELD index, text, vector`: one row per
/// text, in order; `null` texts give `null` vectors.
pub(crate) fn embed_batch(ctx: &ProcedureContext<'_>, args: &[Value]) -> Result<ProcedureRows> {
    const NAME: &str = "vector.embedBatch";
    let items = match &args[0] {
        Value::Null => return Ok(Vec::new()),
        Value::List(l) => l,
        other => return Err(Error::Type(format!("{NAME}: `texts` must be a list, not {}", other.type_name()))),
    };
    let model = model_arg(NAME, args.get(1))?;
    let options = EmbedOptions::from_value(NAME, args.get(2).unwrap_or(&Value::Null))?;
    let mut texts = Vec::with_capacity(items.len());
    for v in items {
        match v {
            Value::Null => {}
            Value::String(s) => texts.push(s.as_str()),
            other => return Err(Error::Type(format!("{NAME}: `texts` must hold strings, not {}", other.type_name()))),
        }
    }
    let embedder = ctx.embedder()?;
    let mut vectors = embed_checked(embedder, model, &texts, &options, &|| ctx.check_interrupt())?.into_iter();
    Ok(items
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let vector = match t {
                Value::Null => Value::Null,
                _ => vector_value(vectors.next().expect("one vector per text")),
            };
            vec![Value::Int(i as i64), t.clone(), vector]
        })
        .collect())
}

/// `CALL vector.models() YIELD name, dimensions, maxTokens, loaded`, by name.
pub(crate) fn models(ctx: &ProcedureContext<'_>, _args: &[Value]) -> Result<ProcedureRows> {
    let mut models = ctx.embedder()?.models();
    models.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(models
        .into_iter()
        .map(|m| {
            vec![
                Value::String(m.name),
                Value::Int(m.dimensions as i64),
                Value::Int(m.max_tokens as i64),
                Value::Bool(m.loaded),
            ]
        })
        .collect())
}

/// A model-free [`Embedder`] for tests: each word (lower-cased, split on whitespace)
/// adds ±1 to one dimension chosen by its hash, and the sum is L2-normalized. Texts with
/// the same words get the same vector, and texts that share words are similar.
///
/// [`EmbedTask::Query`] adds the word `query:`, so queries and documents differ. Each
/// model has a `max_tokens` of 64 words. The first model is the default.
#[derive(Debug)]
pub struct HashEmbedder {
    models: Vec<(String, usize)>,
    max_tokens: usize,
    calls: AtomicUsize,
}

impl HashEmbedder {
    /// An embedder with these `(name, dimensions)` models.
    pub fn new<I, S>(models: I) -> Self
    where
        I: IntoIterator<Item = (S, usize)>,
        S: Into<String>,
    {
        HashEmbedder {
            models: models.into_iter().map(|(n, d)| (n.into(), d)).collect(),
            max_tokens: 64,
            calls: AtomicUsize::new(0),
        }
    }

    /// The same, with another `max_tokens` (in words).
    pub fn with_max_tokens(mut self, max_tokens: usize) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// How many times [`Embedder::embed`] was called.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    fn vector(text: &str, dims: usize, task: EmbedTask, max_tokens: usize, truncate: bool) -> Result<Vec<f32>> {
        let lower = text.to_lowercase();
        let mut words: Vec<&str> = lower.split_whitespace().collect();
        if task == EmbedTask::Query {
            words.insert(0, "query:");
        }
        if words.len() > max_tokens {
            if !truncate {
                return Err(Error::Semantic(format!(
                    "the text has {} tokens, more than the model's {max_tokens} (set truncate: true to cut it)",
                    words.len()
                )));
            }
            words.truncate(max_tokens);
        }
        let mut v = vec![0f32; dims];
        for w in words {
            // FNV-1a.
            let h = w.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3));
            v[(h % dims as u64) as usize] += if h >> 63 == 0 { 1.0 } else { -1.0 };
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            v.iter_mut().for_each(|x| *x /= norm);
        }
        Ok(v)
    }
}

impl Embedder for HashEmbedder {
    fn models(&self) -> Vec<ModelInfo> {
        self.models
            .iter()
            .map(|(name, dims)| ModelInfo { name: name.clone(), dimensions: *dims, max_tokens: self.max_tokens, loaded: true })
            .collect()
    }

    fn embed(
        &self,
        model: Option<&str>,
        texts: &[&str],
        options: &EmbedOptions,
        interrupt: &dyn Fn() -> Result<()>,
    ) -> Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let dims = match model {
            None => self.models.first().map(|m| m.1).ok_or_else(not_available)?,
            Some(name) => self.models.iter().find(|m| m.0 == name).map(|m| m.1).ok_or_else(|| {
                let known: Vec<&str> = self.models.iter().map(|m| m.0.as_str()).collect();
                Error::Semantic(format!("unknown embedding model '{name}' (installed: {})", known.join(", ")))
            })?,
        };
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(32) {
            interrupt()?;
            for t in chunk {
                out.push(Self::vector(t, dims, options.task, self.max_tokens, options.truncate)?);
            }
        }
        Ok(out)
    }
}

/// An option map from pairs, for tests.
#[cfg(test)]
fn options(pairs: &[(&str, Value)]) -> Value {
    Value::Map(pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect::<std::collections::BTreeMap<_, _>>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_parse() {
        let o = EmbedOptions::from_value("f", &options(&[("task", "query".into()), ("truncate", Value::Bool(false))])).unwrap();
        assert_eq!(o, EmbedOptions { task: EmbedTask::Query, truncate: false });
        assert_eq!(EmbedOptions::from_value("f", &Value::Null).unwrap(), EmbedOptions::default());
        assert!(EmbedOptions::from_value("f", &options(&[("task", "x".into())])).is_err());
        assert!(EmbedOptions::from_value("f", &options(&[("other", Value::Int(1))])).is_err());
        assert!(EmbedOptions::from_value("f", &Value::Int(1)).is_err());
    }

    #[test]
    fn hash_vectors_are_normalized_and_stable() {
        let e = HashEmbedder::new([("m", 16)]);
        let v = e.embed(None, &["a b c", "a b c", ""], &EmbedOptions::default(), &|| Ok(())).unwrap();
        assert_eq!(v[0], v[1]);
        assert!((v[0].iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-6);
        assert!(v[2].iter().all(|x| *x == 0.0));
    }
}
