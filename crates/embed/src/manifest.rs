//! `model.toml`, the manifest of a model folder, and the checks on model names.
//!
//! ```toml
//! architecture = "bert"   # required; v1 knows only bert
//! dimensions   = 384      # required; must equal the model's hidden size
//! max_tokens   = 512      # default: the model's max_position_embeddings
//! pooling      = "mean"   # mean | cls
//! normalize    = true     # L2-normalize the vectors
//! query_prefix = ""       # put before texts embedded with {task: 'query'}
//! doc_prefix   = ""       # put before documents (the default task)
//! ```
//!
//! The file is a flat list of `key = value` lines (strings, integers, booleans) with `#`
//! comments; unknown keys are errors, so typos do not go unnoticed.

use std::collections::BTreeMap;

/// The model families the encoder knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Architecture {
    /// BERT and its look-alikes: MiniLM, BGE, E5, GTE (small and base).
    Bert,
}

/// How the token vectors become one vector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pooling {
    /// The average of the token vectors (padding excluded).
    Mean,
    /// The vector of the first token (`[CLS]`).
    Cls,
}

/// What `model.toml` says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub architecture: Architecture,
    pub dimensions: usize,
    /// `None`: the model's own limit.
    pub max_tokens: Option<usize>,
    pub pooling: Pooling,
    pub normalize: bool,
    pub query_prefix: String,
    pub doc_prefix: String,
}

#[derive(Clone, Debug, PartialEq)]
enum Toml {
    Str(String),
    Int(i64),
    Bool(bool),
}

impl Manifest {
    /// Parses the text of a `model.toml`.
    pub fn parse(text: &str) -> Result<Manifest, String> {
        let mut values = read_flat_toml(text)?;
        let mut take = |key: &str| values.remove(key);
        let architecture = match take("architecture") {
            Some(Toml::Str(s)) if s == "bert" => Architecture::Bert,
            Some(Toml::Str(s)) => return Err(format!("unknown architecture `{s}` (known: bert)")),
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
        if let Some(key) = values.keys().next() {
            return Err(format!("unknown key `{key}`"));
        }
        Ok(Manifest { architecture, dimensions, max_tokens, pooling, normalize, query_prefix, doc_prefix })
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
        assert_eq!((m.max_tokens, m.pooling, m.normalize), (None, Pooling::Mean, true));
    }

    #[test]
    fn bad_manifests() {
        for (text, err) in [
            ("dimensions = 8", "`architecture` is missing"),
            ("architecture = \"gpt\"\ndimensions = 8", "unknown architecture"),
            ("architecture = \"bert\"", "`dimensions` is missing"),
            ("architecture = \"bert\"\ndimensions = 0", "positive"),
            ("architecture = \"bert\"\ndimensions = 8\npooling = \"max\"", "`pooling`"),
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
