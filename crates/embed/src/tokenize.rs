//! Text to token ids, with a Hugging Face `tokenizer.json`.

use std::ops::Range;
use std::path::Path;

use tokenizers::utils::truncation::TruncationDirection;
use tokenizers::{Encoding, PostProcessor, Tokenizer};

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
    /// Without truncation or padding: [`TextTokenizer::encode`] cuts, as `transformers`
    /// does (the text's tokens first, then the special tokens are added).
    inner: Tokenizer,
    max_tokens: usize,
    /// How many special tokens a text gets (`[CLS]` and `[SEP]`, `<s>` and `</s>`).
    specials: usize,
    pad_id: u32,
}

impl TextTokenizer {
    pub fn load(file: &Path, max_tokens: usize) -> Result<TextTokenizer, String> {
        let mut inner = Tokenizer::from_file(file).map_err(|e| format!("{}: {e}", file.display()))?;
        inner.with_padding(None);
        inner.with_truncation(None).map_err(|e| format!("{}: {e}", file.display()))?;
        let specials = inner.get_post_processor().map_or(0, |p| p.added_tokens(false));
        if specials >= max_tokens {
            return Err(format!("{}: {specials} special tokens leave no room in {max_tokens}", file.display()));
        }
        let pad_id = inner.token_to_id("[PAD]").or_else(|| inner.token_to_id("<pad>")).unwrap_or(0);
        Ok(TextTokenizer { inner, max_tokens, specials, pad_id })
    }

    /// The ids of the special tokens (`[CLS]`, `<s>`, `</s>`, `<pad>`, `<unk>` …), which
    /// lexical weights leave out.
    pub fn special_ids(&self) -> std::collections::HashSet<u32> {
        let mut ids: std::collections::HashSet<u32> =
            self.inner.get_added_tokens_decoder().iter().filter(|(_, t)| t.special).map(|(&id, _)| id).collect();
        ids.insert(self.pad_id);
        ids
    }

    /// The token of `id` as the vocabulary has it (`▁graph`, `##ing`).
    pub fn token(&self, id: u32) -> Option<String> {
        self.inner.id_to_token(id)
    }

    /// The most tokens of text one input holds: `max_tokens` less the special tokens.
    pub fn text_tokens(&self) -> usize {
        self.max_tokens - self.specials
    }

    fn content(&self, texts: &[&str]) -> Result<Vec<Encoding>, String> {
        let owned: Vec<String> = texts.iter().map(|t| t.to_string()).collect();
        self.inner.encode_batch(owned, false).map_err(|e| format!("tokenizing: {e}"))
    }

    /// The tokens of each text. A text longer than `max_tokens` is cut, or with
    /// `truncate == false` fails.
    pub fn encode(&self, texts: &[&str], truncate: bool) -> Result<Vec<Tokens>, String> {
        self.content(texts)?
            .into_iter()
            .map(|mut e| {
                if e.len() > self.text_tokens() {
                    if !truncate {
                        return Err(format!(
                            "the text is longer than the model's {} tokens (set truncate: true to cut it)",
                            self.max_tokens
                        ));
                    }
                    e.truncate(self.text_tokens(), 0, TruncationDirection::Right);
                    e.get_overflowing_mut().clear();
                }
                let e = self.inner.post_process(e, None, true).map_err(|e| format!("tokenizing: {e}"))?;
                Ok(Tokens { ids: e.get_ids().to_vec(), type_ids: e.get_type_ids().to_vec() })
            })
            .collect()
    }

    /// The tokens of `prefix` + `text` (special tokens left out, nothing cut): how many
    /// begin in the prefix, and the byte range in `text` of each of the others. Tokenized
    /// together, as they are embedded: apart, they may give other tokens.
    pub fn spans(&self, prefix: &str, text: &str) -> Result<(usize, Vec<Range<usize>>), String> {
        let e = self.content(&[&format!("{prefix}{text}")])?.pop().unwrap_or_default();
        let p = prefix.len();
        let in_prefix = e.get_offsets().iter().filter(|o| o.0 < p).count();
        let spans = e.get_offsets()[in_prefix..].iter().map(|&(s, e)| s.saturating_sub(p)..e.saturating_sub(p)).collect();
        Ok((in_prefix, spans))
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
