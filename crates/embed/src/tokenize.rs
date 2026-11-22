//! Text to token ids, with a Hugging Face `tokenizer.json`.

use std::path::Path;

use tokenizers::{Tokenizer, TruncationParams};

/// The token ids of one text, special tokens included, padding not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tokens {
    pub ids: Vec<u32>,
    pub type_ids: Vec<u32>,
}

/// A batch padded to its longest text: row-major `[rows, width]`.
#[derive(Clone, Debug, PartialEq)]
pub struct Padded {
    pub rows: usize,
    pub width: usize,
    pub ids: Vec<i64>,
    pub type_ids: Vec<i64>,
    /// 1 for a token, 0 for padding.
    pub mask: Vec<f32>,
}

/// A model's tokenizer, cutting texts to its `max_tokens`.
pub struct TextTokenizer {
    inner: Tokenizer,
    max_tokens: usize,
    pad_id: u32,
}

impl TextTokenizer {
    pub fn load(file: &Path, max_tokens: usize) -> Result<TextTokenizer, String> {
        let mut inner = Tokenizer::from_file(file).map_err(|e| format!("{}: {e}", file.display()))?;
        inner.with_padding(None);
        inner
            .with_truncation(Some(TruncationParams { max_length: max_tokens, ..Default::default() }))
            .map_err(|e| format!("{}: {e}", file.display()))?;
        let pad_id = inner.token_to_id("[PAD]").or_else(|| inner.token_to_id("<pad>")).unwrap_or(0);
        Ok(TextTokenizer { inner, max_tokens, pad_id })
    }

    /// The tokens of each text. A text longer than `max_tokens` is cut, or with
    /// `truncate == false` fails.
    pub fn encode(&self, texts: &[&str], truncate: bool) -> Result<Vec<Tokens>, String> {
        let owned: Vec<String> = texts.iter().map(|t| t.to_string()).collect();
        let encodings = self.inner.encode_batch(owned, true).map_err(|e| format!("tokenizing: {e}"))?;
        encodings
            .into_iter()
            .map(|e| {
                if !truncate && !e.get_overflowing().is_empty() {
                    return Err(format!(
                        "the text is longer than the model's {} tokens (set truncate: true to cut it)",
                        self.max_tokens
                    ));
                }
                Ok(Tokens { ids: e.get_ids().to_vec(), type_ids: e.get_type_ids().to_vec() })
            })
            .collect()
    }

    /// `batch` padded to its longest text.
    pub fn pad(&self, batch: &[&Tokens]) -> Padded {
        let rows = batch.len();
        let width = batch.iter().map(|t| t.ids.len()).max().unwrap_or(0).max(1);
        let mut p = Padded {
            rows,
            width,
            ids: Vec::with_capacity(rows * width),
            type_ids: Vec::with_capacity(rows * width),
            mask: Vec::with_capacity(rows * width),
        };
        for t in batch {
            let pad = width - t.ids.len();
            p.ids.extend(t.ids.iter().map(|&x| x as i64).chain(std::iter::repeat_n(self.pad_id as i64, pad)));
            p.type_ids.extend(t.type_ids.iter().map(|&x| x as i64).chain(std::iter::repeat_n(0, pad)));
            p.mask.extend(std::iter::repeat_n(1.0, t.ids.len()).chain(std::iter::repeat_n(0.0, pad)));
        }
        p
    }
}
