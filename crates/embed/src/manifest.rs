//! `model.toml`, the manifest of a model folder, and the checks on model names.
//!
//! ```toml
//! architecture = "bert"   # required: bert | xlm-roberta | nomic-bert
//! dimensions   = 384      # required; must equal the model's hidden size
//! max_tokens   = 512      # default: the model's limit
//! pooling      = "mean"   # mean | cls
//! normalize    = true     # L2-normalize the vectors
//! query_prefix = ""       # put before texts embedded with {task: 'query'}
//! doc_prefix   = ""       # put before documents (the default task)
//! matryoshka   = "layer-norm"  # optional: truncate | layer-norm (see below)
//! rotary_scaling = 2      # optional, nomic-bert: inputs past the trained length
//! ```
//!
//! `matryoshka` says the model was trained so that the start of its vector is a good
//! vector too (Matryoshka representation learning), and lets a call ask for fewer
//! dimensions (`{dimensions: n}`). Without it, the model makes only full vectors. How the
//! vector is cut follows the model's own instructions: `truncate` keeps the first `n`
//! values (most such models: mxbai-embed-large, arctic-embed v1.5, gte-v1.5);
//! `layer-norm` first normalizes the pooled vector to mean 0 and variance 1, as
//! nomic-embed-text v1.5 asks. Either way the cut vector is L2-normalized again if
//! `normalize` is on. Full vectors are never changed by it.
//!
//! `rotary_scaling` is nomic-bert's `rotary_scaling_factor` (dynamic NTK): with it, the
//! model takes inputs up to its `n_positions` (8192 for nomic-embed-text v1.5) instead of
//! the 2048 it was trained on, as its model card says (factor 2). Inputs up to the
//! trained length embed as without it. It overrides the `config.json` value.
//!
//! The file is a flat list of `key = value` lines (strings, integers, booleans) with `#`
//! comments; unknown keys are errors, so typos do not go unnoticed.

use std::collections::BTreeMap;

/// The model families the encoder knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Architecture {
    /// BERT and its look-alikes: MiniLM, BGE, E5, GTE (small and base), and multilingual
    /// models that are a BERT with a SentencePiece tokenizer (multilingual-e5-small,
    /// paraphrase-multilingual-MiniLM).
    Bert,
    /// XLM-RoBERTa (multilingual-e5-base and -large, bge-m3): a BERT whose positions
    /// start after the padding id.
    XlmRoberta,
    /// nomic-bert (nomic-embed-text v1 and v1.5): rotary positions, SwiGLU, no biases.
    NomicBert,
}

impl Architecture {
    pub const ALL: [Architecture; 3] = [Architecture::Bert, Architecture::XlmRoberta, Architecture::NomicBert];

    /// Its name in `model.toml`.
    pub fn name(self) -> &'static str {
        match self {
            Architecture::Bert => "bert",
            Architecture::XlmRoberta => "xlm-roberta",
            Architecture::NomicBert => "nomic-bert",
        }
    }
}

/// How the token vectors become one vector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pooling {
    /// The average of the token vectors (padding excluded).
    Mean,
    /// The vector of the first token (`[CLS]`).
    Cls,
}

/// How a Matryoshka model's vector is cut to fewer dimensions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Matryoshka {
    /// The first `n` values.
    Truncate,
    /// The first `n` values after a layer norm (no weights) of the pooled vector:
    /// nomic-embed-text v1.5.
    LayerNorm,
}

impl Matryoshka {
    /// The vector `v` (pooled, not normalized) cut to `dims`, L2-normalized if
    /// `normalize`.
    pub fn cut(self, mut v: Vec<f32>, dims: usize, normalize: bool) -> Vec<f32> {
        if self == Matryoshka::LayerNorm && !v.is_empty() {
            // As torch's layer_norm: biased variance, eps 1e-5, in f32.
            let n = v.len() as f32;
            let mean = v.iter().sum::<f32>() / n;
            let var = v.iter().map(|x| (x - mean) * (x - mean)).sum::<f32>() / n;
            let scale = 1.0 / (var + 1e-5).sqrt();
            v.iter_mut().for_each(|x| *x = (*x - mean) * scale);
        }
        v.truncate(dims);
        if normalize {
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
            v.iter_mut().for_each(|x| *x /= norm);
        }
        v
    }
}

/// What `model.toml` says.
#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    pub architecture: Architecture,
    pub dimensions: usize,
    /// `None`: the model's own limit.
    pub max_tokens: Option<usize>,
    pub pooling: Pooling,
    pub normalize: bool,
    pub query_prefix: String,
    pub doc_prefix: String,
    /// `None`: full vectors only.
    pub matryoshka: Option<Matryoshka>,
    /// `None`: as `config.json` says.
    pub rotary_scaling: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
enum Toml {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl Manifest {
    /// Parses the text of a `model.toml`.
    pub fn parse(text: &str) -> Result<Manifest, String> {
        let mut values = read_flat_toml(text)?;
        let mut take = |key: &str| values.remove(key);
        let architecture = match take("architecture") {
            Some(Toml::Str(s)) => match Architecture::ALL.into_iter().find(|a| a.name() == s) {
                Some(a) => a,
                None => return Err(format!("unknown architecture `{s}` (known: bert, xlm-roberta, nomic-bert)")),
            },
            Some(_) => return Err("`architecture` must be a string".into()),
            None => return Err("`architecture` is missing".into()),
        };
        let positive = |key: &str, v: Option<Toml>| -> Result<Option<usize>, String> {
            match v {
                None => Ok(None),
                Some(Toml::Int(n)) if n > 0 => Ok(Some(n as usize)),
                Some(_) => Err(format!("`{key}` must be a positive integer")),
            }
        };
        let dimensions = positive("dimensions", take("dimensions"))?.ok_or("`dimensions` is missing")?;
        let max_tokens = positive("max_tokens", take("max_tokens"))?;
        let pooling = match take("pooling") {
            None => Pooling::Mean,
            Some(Toml::Str(s)) if s == "mean" => Pooling::Mean,
            Some(Toml::Str(s)) if s == "cls" => Pooling::Cls,
            Some(_) => return Err("`pooling` is \"mean\" or \"cls\"".into()),
        };
        let normalize = match take("normalize") {
            None => true,
            Some(Toml::Bool(b)) => b,
            Some(_) => return Err("`normalize` must be true or false".into()),
        };
        let mut string = |key: &str| match take(key) {
            None => Ok(String::new()),
            Some(Toml::Str(s)) => Ok(s),
            Some(_) => Err(format!("`{key}` must be a string")),
        };
        let query_prefix = string("query_prefix")?;
        let doc_prefix = string("doc_prefix")?;
        let matryoshka = match take("matryoshka") {
            None => None,
            Some(Toml::Str(s)) if s == "truncate" => Some(Matryoshka::Truncate),
            Some(Toml::Str(s)) if s == "layer-norm" => Some(Matryoshka::LayerNorm),
            Some(_) => return Err("`matryoshka` is \"truncate\" or \"layer-norm\"".into()),
        };
        let rotary_scaling = match take("rotary_scaling") {
            None => None,
            Some(Toml::Int(n)) if n >= 1 => Some(n as f64),
            Some(Toml::Float(f)) if f >= 1.0 && f.is_finite() => Some(f),
            Some(_) => return Err("`rotary_scaling` must be a number of at least 1".into()),
        };
        if let Some(key) = values.keys().next() {
            return Err(format!("unknown key `{key}`"));
        }
        Ok(Manifest {
            architecture,
            dimensions,
            max_tokens,
            pooling,
            normalize,
            query_prefix,
            doc_prefix,
            matryoshka,
            rotary_scaling,
        })
    }
}

/// Whether `name` may name a model: 1 to 64 of `a-z 0-9 . _ -`, starting with a letter
/// or digit. Such a name is never a path (no separators, no `..`).
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
}

fn read_flat_toml(text: &str) -> Result<BTreeMap<String, Toml>, String> {
    let mut out = BTreeMap::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = |e: String| format!("line {}: {e}", n + 1);
        if line.starts_with('[') {
            return Err(at("tables are not allowed; the file is a flat list of keys".into()));
        }
        let (key, value) = line.split_once('=').ok_or_else(|| at("expected `key = value`".into()))?;
        let key = key.trim();
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(at(format!("bad key `{key}`")));
        }
        let value = parse_value(value.trim()).map_err(at)?;
        if out.insert(key.to_string(), value).is_some() {
            return Err(at(format!("`{key}` is set twice")));
        }
    }
    Ok(out)
}

fn parse_value(text: &str) -> Result<Toml, String> {
    let comment_only = |rest: &str| {
        let rest = rest.trim();
        if rest.is_empty() || rest.starts_with('#') { Ok(()) } else { Err(format!("unexpected `{rest}` after the value")) }
    };
    if let Some(body) = text.strip_prefix('\'') {
        let end = body.find('\'').ok_or("unterminated string")?;
        comment_only(&body[end + 1..])?;
        return Ok(Toml::Str(body[..end].to_string()));
    }
    if let Some(body) = text.strip_prefix('"') {
        let mut s = String::new();
        let mut chars = body.char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '"' => {
                    comment_only(&body[i + 1..])?;
                    return Ok(Toml::Str(s));
                }
                '\\' => s.push(match chars.next().map(|(_, e)| e) {
                    Some('"') => '"',
                    Some('\\') => '\\',
                    Some('n') => '\n',
                    Some('t') => '\t',
                    Some(e) => return Err(format!("unknown escape `\\{e}`")),
                    None => return Err("unterminated string".into()),
                }),
                c => s.push(c),
            }
        }
        return Err("unterminated string".into());
    }
    match text.split('#').next().unwrap_or_default().trim() {
        "true" => Ok(Toml::Bool(true)),
        "false" => Ok(Toml::Bool(false)),
        w if w.contains(['.', 'e', 'E']) && !w.contains("inf") && !w.contains("nan") => {
            w.replace('_', "").parse::<f64>().map(Toml::Float).map_err(|_| format!("bad value `{w}`"))
        }
        w => w.replace('_', "").parse::<i64>().map(Toml::Int).map_err(|_| format!("bad value `{w}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_and_minimal_manifests() {
        let m = Manifest::parse(
            "# a comment\narchitecture = \"bert\"\ndimensions = 384 # trailing\nmax_tokens = 512\npooling = 'cls'\n\
             normalize = false\nquery_prefix = \"query: \"\ndoc_prefix = \"passage: \"\n",
        )
        .unwrap();
        assert_eq!(m.dimensions, 384);
        assert_eq!(m.max_tokens, Some(512));
        assert_eq!(m.pooling, Pooling::Cls);
        assert!(!m.normalize);
        assert_eq!(m.query_prefix, "query: ");
        assert_eq!(m.doc_prefix, "passage: ");
        let m = Manifest::parse("architecture = \"bert\"\ndimensions = 8").unwrap();
        assert_eq!((m.max_tokens, m.pooling, m.normalize, m.matryoshka, m.rotary_scaling), (None, Pooling::Mean, true, None, None));
        for (text, want) in [("2", 2.0), ("1.5", 1.5), ("4.0", 4.0)] {
            let m = Manifest::parse(&format!("architecture = \"nomic-bert\"\ndimensions = 8\nrotary_scaling = {text}")).unwrap();
            assert_eq!(m.rotary_scaling, Some(want));
        }
        for (text, want) in [("truncate", Matryoshka::Truncate), ("layer-norm", Matryoshka::LayerNorm)] {
            let m = Manifest::parse(&format!("architecture = \"nomic-bert\"\ndimensions = 8\nmatryoshka = \"{text}\"")).unwrap();
            assert_eq!(m.matryoshka, Some(want));
        }
    }

    #[test]
    fn matryoshka_cuts() {
        let v = vec![3.0, 4.0, 12.0, -1.0];
        assert_eq!(Matryoshka::Truncate.cut(v.clone(), 2, false), [3.0, 4.0]);
        assert_eq!(Matryoshka::Truncate.cut(v.clone(), 2, true), [0.6, 0.8]);
        // mean 4.5, variance 22.25.
        let s = (22.25f32 + 1e-5).sqrt();
        let got = Matryoshka::LayerNorm.cut(v.clone(), 3, false);
        for (g, w) in got.iter().zip([-1.5 / s, -0.5 / s, 7.5 / s]) {
            assert!((g - w).abs() < 1e-6, "{got:?}");
        }
        let got = Matryoshka::LayerNorm.cut(v, 2, true);
        let n = (1.5f32 * 1.5 + 0.5 * 0.5).sqrt();
        assert!((got[0] + 1.5 / n).abs() < 1e-6 && (got[1] + 0.5 / n).abs() < 1e-6, "{got:?}");
    }

    #[test]
    fn bad_manifests() {
        for (text, err) in [
            ("dimensions = 8", "`architecture` is missing"),
            ("architecture = \"gpt\"\ndimensions = 8", "unknown architecture"),
            ("architecture = \"bert\"", "`dimensions` is missing"),
            ("architecture = \"bert\"\ndimensions = 0", "positive"),
            ("architecture = \"bert\"\ndimensions = 8\npooling = \"max\"", "`pooling`"),
            ("architecture = \"bert\"\ndimensions = 8\nmatryoshka = true", "`matryoshka`"),
            ("architecture = \"nomic-bert\"\ndimensions = 8\nrotary_scaling = 0.5", "`rotary_scaling`"),
            ("architecture = \"nomic-bert\"\ndimensions = 8\nrotary_scaling = \"2\"", "`rotary_scaling`"),
            ("architecture = \"bert\"\ndimensions = 8.5", "positive integer"),
            ("architecture = \"bert\"\ndimensions = 8\nmatryoshka = \"slice\"", "`matryoshka`"),
            ("architecture = \"bert\"\ndimensions = 8\ndims = 8", "unknown key `dims`"),
            ("architecture = \"bert\"\ndimensions = 8\ndimensions = 9", "set twice"),
            ("[model]\narchitecture = \"bert\"", "tables"),
            ("architecture = \"bert", "unterminated"),
            ("architecture", "key = value"),
        ] {
            let e = Manifest::parse(text).unwrap_err();
            assert!(e.contains(err), "{text:?}: {e}");
        }
    }

    #[test]
    fn names() {
        for ok in ["minilm", "bge-small-en-v1.5", "e5_base", "0model"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", ".", "..", ".hidden", "-x", "MiniLM", "a/b", "a\\b", "a b", "ü", &"x".repeat(65)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }
}
