//! Text embeddings: `vector.embed(text, model?, options?)`, `CALL vector.embedBatch(texts,
//! model?, options?)`, `CALL vector.chunk(text, model?, options?)`, `CALL
//! vector.embedChunks(text, model?, options?)` and `CALL vector.models()`.
//!
//! The engine does not run models itself. An [`Embedder`] does, set with
//! [`crate::Database::set_embedder`] or [`crate::DatabaseManager::set_embedder`], much
//! like the kernel backend. Without one, the functions and procedures fail with
//! `Unsupported`. [`HashEmbedder`] is a model-free embedder for tests.
//!
//! The result of an embedding is a plain `LIST<FLOAT>`. It can be stored as a property and
//! indexed by a vector index whose `vector.dimensions` equal the model's. A Matryoshka
//! model (one trained so that the start of its vector is a good vector too) also makes
//! shorter vectors on request: `{dimensions: n}` in the options.
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

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use graphbuff_core::{Error, Result, Value};

use crate::procedures::{ProcedureContext, ProcedureRows};

/// An installed embedding model, as `vector.models()` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    pub name: String,
    /// The length of the vectors it returns (unless asked for fewer).
    pub dimensions: usize,
    /// The longest input it takes, in tokens.
    pub max_tokens: usize,
    /// Whether it is loaded in memory now.
    pub loaded: bool,
    /// Whether it makes shorter vectors too (`{dimensions: n}`, `n` up to `dimensions`):
    /// a Matryoshka model.
    pub matryoshka: bool,
}

impl ModelInfo {
    /// Whether it makes vectors of `dimensions`: its own, or fewer if it is Matryoshka.
    pub fn fits(&self, dimensions: usize) -> bool {
        dimensions == self.dimensions || (self.matryoshka && (1..self.dimensions).contains(&dimensions))
    }
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
/// (`{task: 'query' | 'document', truncate: true | false, dimensions: n}`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmbedOptions {
    pub task: EmbedTask,
    /// Whether text longer than the model's `max_tokens` is cut (`true`, the default)
    /// or fails.
    pub truncate: bool,
    /// The length of the vectors; `None` (the default): the model's own. Fewer than the
    /// model's needs a Matryoshka model ([`ModelInfo::matryoshka`]); the model's own
    /// always works.
    pub dimensions: Option<usize>,
}

impl Default for EmbedOptions {
    fn default() -> Self {
        EmbedOptions { task: EmbedTask::Document, truncate: true, dimensions: None }
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
                ("dimensions", Value::Int(n)) if *n >= 1 => o.dimensions = Some(*n as usize),
                ("dimensions", Value::Int(n)) => {
                    return Err(Error::Semantic(format!("{what}: `dimensions` must be at least 1, not {n}")));
                }
                ("task" | "truncate" | "dimensions", Value::Null) => {}
                ("task" | "truncate" | "dimensions", other) => {
                    return Err(Error::Type(format!("{what}: `{k}` does not accept {}", other.type_name())));
                }
                _ => {
                    return Err(Error::Semantic(format!(
                        "{what}: unknown option `{k}` (known: task, truncate, dimensions)"
                    )));
                }
            }
        }
        Ok(o)
    }
}

/// The options of `vector.chunk` and `vector.embedChunks`: `{size, overlap, task,
/// dimensions}` (`dimensions` as in [`EmbedOptions`], for `embedChunks`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChunkOptions {
    /// Tokens per chunk at most; `None`: as many as fit the model.
    pub size: Option<usize>,
    /// Tokens a chunk repeats from the end of the one before (it may start a little
    /// later, at a word).
    pub overlap: usize,
    /// What the chunks will be embedded as: its prefix takes room too.
    pub task: EmbedTask,
    /// The length of the chunks' vectors; `None`: the model's own.
    pub dimensions: Option<usize>,
}

impl ChunkOptions {
    pub(crate) fn from_value(what: &str, v: &Value) -> Result<Self> {
        let mut o = ChunkOptions::default();
        let map = match v {
            Value::Null => return Ok(o),
            Value::Map(m) => m,
            other => return Err(Error::Type(format!("{what}: options must be a map, not {}", other.type_name()))),
        };
        let count = |k: &str, v: &Value, least: i64| match v {
            Value::Int(n) if *n >= least => Ok(*n as usize),
            Value::Int(n) => Err(Error::Semantic(format!("{what}: `{k}` must be at least {least}, not {n}"))),
            other => Err(Error::Type(format!("{what}: `{k}` must be an integer, not {}", other.type_name()))),
        };
        for (k, v) in map {
            match (k.as_str(), v) {
                (_, Value::Null) if matches!(k.as_str(), "size" | "overlap" | "task" | "dimensions") => {}
                ("size", v) => o.size = Some(count(k, v, 1)?),
                ("overlap", v) => o.overlap = count(k, v, 0)?,
                ("task" | "dimensions", _) => {
                    let single = Value::Map([(k.clone(), v.clone())].into_iter().collect());
                    let e = EmbedOptions::from_value(what, &single)?;
                    match k.as_str() {
                        "task" => o.task = e.task,
                        _ => o.dimensions = e.dimensions,
                    }
                }
                _ => {
                    return Err(Error::Semantic(format!(
                        "{what}: unknown option `{k}` (known: size, overlap, task, dimensions)"
                    )));
                }
            }
        }
        if let Some(size) = o.size
            && o.overlap >= size
        {
            return Err(Error::Semantic(format!("{what}: `overlap` ({}) must be less than `size` ({size})", o.overlap)));
        }
        Ok(o)
    }
}

/// How a model splits a text into tokens, for chunking.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TokenSpans {
    /// Each token's byte range in the text, in order; special tokens and prefixes are not
    /// in it.
    pub spans: Vec<Range<usize>>,
    /// How many of these tokens one input holds: the model's limit less its special
    /// tokens and the prefix of the task.
    pub fit: usize,
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
    /// stop with its error. An unknown model fails with `Semantic`, and so does
    /// `options.dimensions` that the model does not [fit](ModelInfo::fits).
    fn embed(
        &self,
        model: Option<&str>,
        texts: &[&str],
        options: &EmbedOptions,
        interrupt: &dyn Fn() -> Result<()>,
    ) -> Result<Vec<Vec<f32>>>;

    /// The tokens of `text` for `model` (`None`: the default), and how many fit in one
    /// input of `task`: what `vector.chunk` cuts text by. Embedders that cannot tell
    /// fail with `Unsupported`.
    fn token_spans(&self, model: Option<&str>, text: &str, task: EmbedTask) -> Result<TokenSpans> {
        let _ = (model, text, task);
        Err(Error::Unsupported("this embedder cannot split text into chunks".into()))
    }
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
    if let Some(d) = options.dimensions
        && let Some(v) = out.iter().find(|v| v.len() != d)
    {
        return Err(Error::Runtime(format!("the embedder returned a vector of {} dimensions, not the {d} asked for", v.len())));
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

/// `CALL vector.models() YIELD name, dimensions, maxTokens, loaded, matryoshka`, by name.
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
                Value::Bool(m.matryoshka),
            ]
        })
        .collect())
}

/// A piece of a text, as `vector.chunk` gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// Its byte range in the text (whitespace at either end left out).
    pub bytes: Range<usize>,
    /// Its tokens, as counted in the whole text.
    pub tokens: usize,
}

/// How good a place to cut the text before token `k` is: 3 a paragraph, 2 a sentence or
/// line, 1 a word, 0 inside a word.
fn break_quality(text: &str, spans: &[Range<usize>], k: usize) -> u8 {
    if k == 0 || k >= spans.len() {
        return 3;
    }
    let (prev, next) = (&spans[k - 1], &spans[k]);
    let gap = text.get(prev.end..next.start.max(prev.end)).unwrap_or("");
    // Some tokenizers (SentencePiece) give the space to the token after it.
    let own = text.get(next.clone()).unwrap_or("");
    let lead = &own[..own.len() - own.trim_start().len()];
    let space: String = [gap, lead].concat();
    if space.matches('\n').count() >= 2 {
        3
    } else if space.contains('\n')
        || (!space.is_empty()
            && text.get(..prev.end).and_then(|t| t.trim_end().chars().last()).is_some_and(|c| ".!?…。！？".contains(c)))
    {
        2
    } else if space.chars().any(char::is_whitespace) {
        1
    } else {
        0
    }
}

/// `text` (whose tokens are `spans`) in chunks of at most `size` tokens, each repeating
/// about `overlap` tokens of the one before. A chunk ends at the best break (paragraph,
/// sentence, word) in the second half of its room; inside a word only when there is
/// none. Chunks of whitespace only are left out.
pub fn chunk_spans(text: &str, spans: &[Range<usize>], size: usize, overlap: usize) -> Vec<Chunk> {
    let (n, size) = (spans.len(), size.max(1));
    let overlap = overlap.min(size - 1);
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let hi = (i + size).min(n);
        let end = if hi == n {
            n
        } else {
            let lo = i + (size / 2).max(1);
            let mut best = (0, hi);
            for k in (lo..=hi).rev() {
                let q = break_quality(text, spans, k);
                if q > best.0 {
                    best = (q, k);
                    if q == 3 {
                        break;
                    }
                }
            }
            best.1
        };
        let (from, to) = (spans[i].start, spans[end - 1].end.max(spans[i].start));
        let piece = text.get(from..to).unwrap_or("");
        let start = from + (piece.len() - piece.trim_start().len());
        let stop = start + piece.trim().len();
        if stop > start {
            out.push(Chunk { bytes: start..stop, tokens: end - i });
        }
        if end == n {
            break;
        }
        // The next chunk starts `overlap` tokens back, at the start of a word.
        let mut next = end.saturating_sub(overlap).max(i + 1);
        while next < end && break_quality(text, spans, next) == 0 {
            next += 1;
        }
        i = next;
    }
    out
}

/// A text, its chunks, and the model and options they were made for.
type Chunked<'a> = (&'a str, Vec<Chunk>, Option<&'a str>, ChunkOptions);

/// The chunks of `args` (`text, model, options`) and the options; `None` for a `null`
/// text.
fn chunks_of<'a>(what: &str, embedder: &dyn Embedder, args: &'a [Value]) -> Result<Option<Chunked<'a>>> {
    let text = match &args[0] {
        Value::Null => return Ok(None),
        Value::String(s) => s.as_str(),
        other => return Err(Error::Type(format!("{what}: `text` must be a string, not {}", other.type_name()))),
    };
    let model = model_arg(what, args.get(1))?;
    let options = ChunkOptions::from_value(what, args.get(2).unwrap_or(&Value::Null))?;
    let tokens = embedder.token_spans(model, text, options.task)?;
    if tokens.fit == 0 {
        return Err(Error::Semantic(format!("{what}: no text fits the model with its prefix")));
    }
    let size = match options.size {
        Some(s) if s > tokens.fit => {
            return Err(Error::Semantic(format!(
                "{what}: `size` {s} is more than the {} tokens of text that fit the model",
                tokens.fit
            )));
        }
        Some(s) => s,
        None => tokens.fit,
    };
    if options.overlap >= size {
        return Err(Error::Semantic(format!("{what}: `overlap` ({}) must be less than the size ({size})", options.overlap)));
    }
    let mut out = Vec::new();
    for c in chunk_spans(text, &tokens.spans, size, options.overlap) {
        fitted(embedder, model, options.task, text, c, size, 2, &mut out)?;
    }
    Ok(Some((text, out, model, options)))
}

/// `chunk` with the tokens it has on its own after the prefix, which a tokenizer may
/// count differently than inside the whole text (SentencePiece does): split again if it
/// does not fit in `size` or the model, at most `depth` more times.
#[allow(clippy::too_many_arguments)]
fn fitted(
    embedder: &dyn Embedder,
    model: Option<&str>,
    task: EmbedTask,
    text: &str,
    chunk: Chunk,
    size: usize,
    depth: usize,
    out: &mut Vec<Chunk>,
) -> Result<()> {
    let piece = &text[chunk.bytes.clone()];
    let own = embedder.token_spans(model, piece, task)?;
    let size = size.min(own.fit).max(1);
    if own.spans.len() <= size || depth == 0 {
        out.push(Chunk { tokens: own.spans.len(), ..chunk });
        return Ok(());
    }
    for c in chunk_spans(piece, &own.spans, size, 0) {
        let at = chunk.bytes.start;
        fitted(embedder, model, task, text, Chunk { bytes: at + c.bytes.start..at + c.bytes.end, tokens: c.tokens }, size, depth - 1, out)?;
    }
    Ok(())
}

/// The rows `index, text, start, length, tokens` of `chunks`; `start` and `length` count
/// characters, as `substring()` does.
fn chunk_rows(text: &str, chunks: &[Chunk]) -> Vec<Vec<Value>> {
    let (mut byte, mut chars) = (0, 0);
    chunks
        .iter()
        .enumerate()
        .map(|(i, c)| {
            // Chunks start in order, so counting goes on from the last one.
            chars += text[byte..c.bytes.start].chars().count();
            byte = c.bytes.start;
            let piece = &text[c.bytes.clone()];
            vec![
                Value::Int(i as i64),
                Value::String(piece.to_string()),
                Value::Int(chars as i64),
                Value::Int(piece.chars().count() as i64),
                Value::Int(c.tokens as i64),
            ]
        })
        .collect()
}

/// `CALL vector.chunk(text, model, options) YIELD index, text, start, length, tokens`.
pub(crate) fn chunk(ctx: &ProcedureContext<'_>, args: &[Value]) -> Result<ProcedureRows> {
    let Some((text, chunks, _, _)) = chunks_of("vector.chunk", ctx.embedder()?, args)? else { return Ok(Vec::new()) };
    Ok(chunk_rows(text, &chunks))
}

/// `CALL vector.embedChunks(text, model, options) YIELD index, text, start, length, tokens,
/// vector`.
pub(crate) fn embed_chunks(ctx: &ProcedureContext<'_>, args: &[Value]) -> Result<ProcedureRows> {
    let embedder = ctx.embedder()?;
    let Some((text, chunks, model, options)) = chunks_of("vector.embedChunks", embedder, args)? else {
        return Ok(Vec::new());
    };
    let pieces: Vec<&str> = chunks.iter().map(|c| &text[c.bytes.clone()]).collect();
    let embed = EmbedOptions { task: options.task, truncate: true, dimensions: options.dimensions };
    let vectors = embed_checked(embedder, model, &pieces, &embed, &|| ctx.check_interrupt())?;
    Ok(chunk_rows(text, &chunks)
        .into_iter()
        .zip(vectors)
        .map(|(mut row, v)| {
            row.push(vector_value(v));
            row
        })
        .collect())
}

/// A model-free [`Embedder`] for tests: each word (lower-cased, split on whitespace)
/// adds ±1 to one dimension chosen by its hash, and the sum is L2-normalized. Texts with
/// the same words get the same vector, and texts that share words are similar.
///
/// [`EmbedTask::Query`] adds the word `query:`, so queries and documents differ. Each
/// model has a `max_tokens` of 64 words. The first model is the default. With
/// [`matryoshka`](Self::matryoshka) its models also make fewer dimensions: the first
/// ones, normalized again.
#[derive(Debug)]
pub struct HashEmbedder {
    models: Vec<(String, usize)>,
    max_tokens: usize,
    matryoshka: bool,
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
            matryoshka: false,
            calls: AtomicUsize::new(0),
        }
    }

    /// The same, with another `max_tokens` (in words).
    pub fn with_max_tokens(mut self, max_tokens: usize) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// The same, with Matryoshka models (`{dimensions: n}` up to theirs).
    pub fn matryoshka(mut self) -> Self {
        self.matryoshka = true;
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
            .map(|(name, dims)| ModelInfo {
                name: name.clone(),
                dimensions: *dims,
                max_tokens: self.max_tokens,
                loaded: true,
                matryoshka: self.matryoshka,
            })
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
        let cut = match options.dimensions {
            None => dims,
            Some(d) if d == dims || (self.matryoshka && (1..dims).contains(&d)) => d,
            Some(d) => return Err(Error::Semantic(format!("the model makes vectors of {dims} dimensions, not {d}"))),
        };
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(32) {
            interrupt()?;
            for t in chunk {
                let mut v = Self::vector(t, dims, options.task, self.max_tokens, options.truncate)?;
                if cut < dims {
                    v.truncate(cut);
                    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                    if norm > 0.0 {
                        v.iter_mut().for_each(|x| *x /= norm);
                    }
                }
                out.push(v);
            }
        }
        Ok(out)
    }

    /// Its tokens are the words (split on whitespace); a query's `query:` takes one.
    fn token_spans(&self, model: Option<&str>, text: &str, task: EmbedTask) -> Result<TokenSpans> {
        if let Some(name) = model
            && !self.models.iter().any(|m| m.0 == name)
        {
            return Err(Error::Semantic(format!("unknown embedding model '{name}'")));
        }
        let base = text.as_ptr() as usize;
        let spans = text.split_whitespace().map(|w| (w.as_ptr() as usize - base)..(w.as_ptr() as usize - base + w.len())).collect();
        let fit = self.max_tokens - usize::from(task == EmbedTask::Query).min(self.max_tokens);
        Ok(TokenSpans { spans, fit })
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
        assert_eq!(o, EmbedOptions { task: EmbedTask::Query, truncate: false, dimensions: None });
        let o = EmbedOptions::from_value("f", &options(&[("dimensions", Value::Int(64))])).unwrap();
        assert_eq!(o.dimensions, Some(64));
        assert!(EmbedOptions::from_value("f", &options(&[("dimensions", Value::Int(0))])).is_err());
        assert!(EmbedOptions::from_value("f", &options(&[("dimensions", "x".into())])).is_err());
        assert_eq!(EmbedOptions::from_value("f", &Value::Null).unwrap(), EmbedOptions::default());
        assert!(EmbedOptions::from_value("f", &options(&[("task", "x".into())])).is_err());
        assert!(EmbedOptions::from_value("f", &options(&[("other", Value::Int(1))])).is_err());
        assert!(EmbedOptions::from_value("f", &Value::Int(1)).is_err());
    }

    fn words(text: &str) -> Vec<Range<usize>> {
        HashEmbedder::new([("m", 4)]).token_spans(None, text, EmbedTask::Document).unwrap().spans
    }

    fn pieces(text: &str, size: usize, overlap: usize) -> Vec<String> {
        chunk_spans(text, &words(text), size, overlap).iter().map(|c| text[c.bytes.clone()].to_string()).collect()
    }

    #[test]
    fn chunks_end_at_the_best_break() {
        assert_eq!(pieces("just a few words", 8, 0), ["just a few words"]);
        assert!(pieces("", 8, 0).is_empty());
        assert!(pieces(" \n\t ", 8, 0).is_empty());
        let text = "One two three. Four five six. Seven eight nine.";
        assert_eq!(pieces(text, 5, 0), ["One two three.", "Four five six.", "Seven eight nine."], "sentences");
        let text = "a b c\n\nd e f. g h i j";
        assert_eq!(pieces(text, 6, 0), ["a b c", "d e f.", "g h i j"], "a paragraph before a sentence");
        let text = "a b c d\ne f g h";
        assert_eq!(pieces(text, 6, 0), ["a b c d", "e f g h"], "a line");
        // No break in the second half of the room: cut inside the word.
        let text = "abcdefghij";
        let chars: Vec<Range<usize>> = (0..text.len()).map(|i| i..i + 1).collect();
        let got: Vec<&str> = chunk_spans(text, &chars, 4, 0).iter().map(|c| &text[c.bytes.clone()]).collect();
        assert_eq!(got, ["abcd", "efgh", "ij"]);
    }

    #[test]
    fn chunks_cover_the_text_with_their_overlap() {
        let text: String = (0..40).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ");
        let spans = words(&text);
        for (size, overlap) in [(8, 0), (8, 3), (5, 4), (40, 0), (100, 10)] {
            let chunks = chunk_spans(&text, &spans, size, overlap);
            assert!(chunks.iter().all(|c| c.tokens <= size && c.tokens > 0), "{size}/{overlap}");
            assert_eq!(chunks[0].bytes.start, 0);
            assert_eq!(chunks.last().unwrap().bytes.end, text.len());
            for w in chunks.windows(2) {
                let (a, b) = (&w[0].bytes, &w[1].bytes);
                assert!(b.start > a.start && b.start <= a.end + 1, "in order, no gap: {a:?} {b:?}");
                let shared = text[b.start..a.end.max(b.start)].split_whitespace().count();
                assert_eq!(shared, overlap.min(size - 1), "{size}/{overlap}: {a:?} {b:?}");
            }
        }
    }

    #[test]
    fn chunk_options_parse() {
        let o = ChunkOptions::from_value(
            "f",
            &options(&[("size", Value::Int(100)), ("overlap", Value::Int(10)), ("task", "query".into())]),
        )
        .unwrap();
        assert_eq!(o, ChunkOptions { size: Some(100), overlap: 10, task: EmbedTask::Query, dimensions: None });
        let o = ChunkOptions::from_value("f", &options(&[("dimensions", Value::Int(8)), ("task", "query".into())])).unwrap();
        assert_eq!((o.task, o.dimensions), (EmbedTask::Query, Some(8)));
        assert_eq!(ChunkOptions::from_value("f", &Value::Null).unwrap(), ChunkOptions::default());
        for (bad, err) in [
            (options(&[("size", Value::Int(0))]), "at least 1"),
            (options(&[("overlap", Value::Int(-1))]), "at least 0"),
            (options(&[("size", Value::Int(4)), ("overlap", Value::Int(4))]), "less than `size`"),
            (options(&[("size", "x".into())]), "integer"),
            (options(&[("truncate", Value::Bool(true))]), "unknown option `truncate`"),
            (options(&[("task", "x".into())]), "'query' or 'document'"),
        ] {
            let e = ChunkOptions::from_value("f", &bad).unwrap_err().to_string();
            assert!(e.contains(err), "{e}");
        }
    }

    #[test]
    fn hash_vectors_are_normalized_and_stable() {
        let e = HashEmbedder::new([("m", 16)]);
        let v = e.embed(None, &["a b c", "a b c", ""], &EmbedOptions::default(), &|| Ok(())).unwrap();
        assert_eq!(v[0], v[1]);
        assert!((v[0].iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-6);
        assert!(v[2].iter().all(|x| *x == 0.0));
        let cut = EmbedOptions { dimensions: Some(4), ..Default::default() };
        assert!(e.embed(None, &["a"], &cut, &|| Ok(())).is_err(), "not Matryoshka");
        let e = HashEmbedder::new([("m", 16)]).matryoshka();
        let short = e.embed(None, &["a b c d e f g"], &cut, &|| Ok(())).unwrap();
        assert_eq!(short[0].len(), 4);
        assert!(e.embed(None, &["a"], &EmbedOptions { dimensions: Some(17), ..Default::default() }, &|| Ok(())).is_err());
    }
}
