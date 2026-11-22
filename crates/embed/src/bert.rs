//! A BERT encoder in Burn, with weights read straight from a Hugging Face
//! `model.safetensors` (`BertModel` key names, with or without a `bert.` prefix).
//!
//! embeddings (word + position + token type, LayerNorm) → N × (self-attention, add &
//! norm, GELU feed-forward, add & norm) → pooling (mean over the mask, or `[CLS]`) →
//! optional L2 normalization. Matches `transformers` to about 1e-6 (tests/golden.rs).

use std::path::Path;

use burn::tensor::{Device, Int, Tensor, TensorData, activation};

use crate::manifest::Pooling;
use crate::registry::BertConfig;
use crate::tokenize::Padded;

struct Linear {
    /// `[in, out]` (Hugging Face stores `[out, in]`).
    w: Tensor<2>,
    b: Tensor<1>,
}

impl Linear {
    fn forward(&self, x: Tensor<3>) -> Tensor<3> {
        let [b, t, i] = x.dims();
        let out = self.w.dims()[1];
        (x.reshape([b * t, i]).matmul(self.w.clone()) + self.b.clone().unsqueeze::<2>()).reshape([b, t, out])
    }
}

struct LayerNorm {
    w: Tensor<1>,
    b: Tensor<1>,
    eps: f64,
}

impl LayerNorm {
    fn forward(&self, x: Tensor<3>) -> Tensor<3> {
        let mean = x.clone().mean_dim(2);
        let xc = x - mean;
        let var = xc.clone().powf_scalar(2.0).mean_dim(2);
        xc / (var + self.eps).sqrt() * self.w.clone().unsqueeze::<3>() + self.b.clone().unsqueeze::<3>()
    }
}

struct Layer {
    q: Linear,
    k: Linear,
    v: Linear,
    attn_out: Linear,
    attn_norm: LayerNorm,
    inter: Linear,
    out: Linear,
    out_norm: LayerNorm,
}

/// A loaded BERT model.
pub struct Bert {
    config: BertConfig,
    device: Device,
    word: Tensor<2>,
    pos: Tensor<2>,
    typ: Tensor<2>,
    emb_norm: LayerNorm,
    layers: Vec<Layer>,
}

struct Weights<'a> {
    st: safetensors::SafeTensors<'a>,
    device: &'a Device,
}

impl Weights<'_> {
    fn tensor<const D: usize>(&self, name: &str, shape: [usize; D]) -> Result<Tensor<D>, String> {
        let view = self
            .st
            .tensor(name)
            .or_else(|_| self.st.tensor(&format!("bert.{name}")))
            .map_err(|_| format!("the weights have no `{name}`"))?;
        if view.dtype() != safetensors::Dtype::F32 {
            return Err(format!("`{name}` is {:?}; only F32 weights are supported", view.dtype()));
        }
        if view.shape() != shape {
            return Err(format!("`{name}` has shape {:?}, expected {shape:?}", view.shape()));
        }
        let vals: Vec<f32> =
            view.data().as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect();
        Ok(Tensor::from_data(TensorData::new(vals, shape.to_vec()), self.device))
    }

    fn linear(&self, prefix: &str, input: usize, output: usize) -> Result<Linear, String> {
        Ok(Linear {
            w: self.tensor(&format!("{prefix}.weight"), [output, input])?.transpose(),
            b: self.tensor(&format!("{prefix}.bias"), [output])?,
        })
    }

    fn norm(&self, prefix: &str, size: usize, eps: f64) -> Result<LayerNorm, String> {
        Ok(LayerNorm {
            w: self.tensor(&format!("{prefix}.weight"), [size])?,
            b: self.tensor(&format!("{prefix}.bias"), [size])?,
            eps,
        })
    }
}

impl Bert {
    /// Loads `model.safetensors` from `file` onto `device`.
    pub fn load(file: &Path, config: &BertConfig, device: &Device) -> Result<Bert, String> {
        let bytes = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let st = safetensors::SafeTensors::deserialize(&bytes).map_err(|e| format!("{}: {e}", file.display()))?;
        let w = Weights { st, device };
        let (h, ff, eps) = (config.hidden_size, config.intermediate_size, config.layer_norm_eps);
        let layers = (0..config.num_hidden_layers)
            .map(|i| {
                let p = format!("encoder.layer.{i}");
                Ok(Layer {
                    q: w.linear(&format!("{p}.attention.self.query"), h, h)?,
                    k: w.linear(&format!("{p}.attention.self.key"), h, h)?,
                    v: w.linear(&format!("{p}.attention.self.value"), h, h)?,
                    attn_out: w.linear(&format!("{p}.attention.output.dense"), h, h)?,
                    attn_norm: w.norm(&format!("{p}.attention.output.LayerNorm"), h, eps)?,
                    inter: w.linear(&format!("{p}.intermediate.dense"), h, ff)?,
                    out: w.linear(&format!("{p}.output.dense"), ff, h)?,
                    out_norm: w.norm(&format!("{p}.output.LayerNorm"), h, eps)?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Bert {
            word: w.tensor("embeddings.word_embeddings.weight", [config.vocab_size, h])?,
            pos: w.tensor("embeddings.position_embeddings.weight", [config.max_position_embeddings, h])?,
            typ: w.tensor("embeddings.token_type_embeddings.weight", [config.type_vocab_size, h])?,
            emb_norm: w.norm("embeddings.LayerNorm", h, eps)?,
            layers,
            config: config.clone(),
            device: device.clone(),
        })
    }

    /// About the most memory a forward pass of `rows` × `width` tokens holds at once:
    /// the hidden states and their copies, the feed-forward layer, and the attention
    /// scores (`heads × width²` per row), in f32.
    pub fn activation_bytes(&self, rows: usize, width: usize) -> usize {
        let c = &self.config;
        let per_token = 6 * c.hidden_size + 2 * c.intermediate_size + 2 * c.num_attention_heads * width;
        4usize.saturating_mul(rows).saturating_mul(width).saturating_mul(per_token)
    }

    /// The vectors of a padded batch, one per row.
    pub fn embed(&self, batch: &Padded, pooling: Pooling, normalize: bool) -> Result<Vec<Vec<f32>>, String> {
        let (b, t, h) = (batch.rows, batch.width, self.config.hidden_size);
        if t > self.config.max_position_embeddings {
            return Err(format!("{t} tokens is more than the model's {}", self.config.max_position_embeddings));
        }
        let vocab = self.config.vocab_size as i64;
        if let Some(bad) = batch.ids.iter().find(|&&i| i < 0 || i >= vocab) {
            return Err(format!("token id {bad} is outside the model's vocabulary of {vocab}"));
        }
        if let Some(bad) = batch.type_ids.iter().find(|&&i| i < 0 || i >= self.config.type_vocab_size as i64) {
            return Err(format!("token type id {bad} is outside the model's {}", self.config.type_vocab_size));
        }
        let dev = &self.device;
        let heads = self.config.num_attention_heads;
        let hd = h / heads;
        let ids = Tensor::<1, Int>::from_data(TensorData::new(batch.ids.clone(), [b * t]), dev);
        let types = Tensor::<1, Int>::from_data(TensorData::new(batch.type_ids.clone(), [b * t]), dev);
        let pos = self.pos.clone().select(0, Tensor::<1, Int>::arange(0..t as i64, dev)).unsqueeze::<3>();
        let x = self.word.clone().select(0, ids).reshape([b, t, h])
            + self.typ.clone().select(0, types).reshape([b, t, h])
            + pos;
        let mut x = self.emb_norm.forward(x);

        let mask = Tensor::<2>::from_data(TensorData::new(batch.mask.clone(), [b, t]), dev);
        // Added to the attention scores: 0 for tokens, very negative for padding.
        let bias = (mask.clone().neg() + 1.0).mul_scalar(-1e9).reshape([b, 1, 1, t]);
        let scale = 1.0 / (hd as f64).sqrt();
        let split = |y: Tensor<3>| y.reshape([b, t, heads, hd]).swap_dims(1, 2);
        for l in &self.layers {
            let q = split(l.q.forward(x.clone()));
            let k = split(l.k.forward(x.clone()));
            let v = split(l.v.forward(x.clone()));
            let scores = q.matmul(k.transpose()).mul_scalar(scale) + bias.clone();
            let ctx = activation::softmax(scores, 3).matmul(v).swap_dims(1, 2).reshape([b, t, h]);
            x = l.attn_norm.forward(l.attn_out.forward(ctx) + x);
            let ff = l.out.forward(activation::gelu(l.inter.forward(x.clone())));
            x = l.out_norm.forward(ff + x);
        }

        let pooled = match pooling {
            Pooling::Mean => {
                let m = mask.reshape([b, t, 1]);
                let summed = (x * m.clone()).sum_dim(1).reshape([b, h]);
                summed / m.sum_dim(1).reshape([b, 1]).clamp_min(1e-9)
            }
            Pooling::Cls => x.narrow(1, 0, 1).reshape([b, h]),
        };
        let pooled = if normalize {
            let norm = pooled.clone().powf_scalar(2.0).sum_dim(1).sqrt().clamp_min(1e-12);
            pooled / norm
        } else {
            pooled
        };
        let flat: Vec<f32> = pooled.into_data().try_to_vec().map_err(|e| format!("reading the result: {e:?}"))?;
        Ok(flat.chunks_exact(h).map(<[f32]>::to_vec).collect())
    }
}
