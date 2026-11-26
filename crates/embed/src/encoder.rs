//! The encoder models in Burn, with weights read straight from a Hugging Face
//! `model.safetensors` (F32, F16 or BF16; half-precision weights are widened to f32 as
//! they load):
//!
//! - BERT (`BertModel` key names, with or without a `bert.` prefix) and XLM-RoBERTa (the
//!   same names, perhaps with a `roberta.` prefix; positions start after the padding id);
//! - nomic-bert (`NomicBertModel`: one `Wqkv` projection, rotary positions, SwiGLU, no
//!   biases).
//!
//! embeddings (word + token type [+ position], gathered on the host; LayerNorm) → N ×
//! (self-attention [rotary on queries and keys], add & norm, feed-forward [GELU or
//! SwiGLU], add & norm) → pooling (mean over the mask, or the first token) → optional L2
//! normalization. Matches `transformers` to about 1e-6 (tests/golden.rs).

use std::path::Path;

use burn::tensor::{Device, Tensor, TensorData, activation, bf16, f16};

use crate::manifest::{Architecture, Pooling};
use crate::registry::{FeedForward, ModelConfig, Positions};
use crate::tokenize::Padded;

struct Linear {
    /// `[in, out]` (Hugging Face stores `[out, in]`).
    w: Tensor<2>,
    b: Option<Tensor<1>>,
}

impl Linear {
    fn forward(&self, x: Tensor<3>) -> Tensor<3> {
        let [b, t, i] = x.dims();
        let out = self.w.dims()[1];
        let y = x.reshape([b * t, i]).matmul(self.w.clone());
        let y = match &self.b {
            Some(bias) => y + bias.clone().unsqueeze::<2>(),
            None => y,
        };
        y.reshape([b, t, out])
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

// One per layer: boxing the larger variant would save nothing worth an indirection.
#[allow(clippy::large_enum_variant)]
enum Ffn {
    Gelu { inner: Linear, out: Linear },
    SwiGlu { up: Linear, gate: Linear, down: Linear },
}

impl Ffn {
    fn forward(&self, x: Tensor<3>) -> Tensor<3> {
        match self {
            Ffn::Gelu { inner, out } => out.forward(activation::gelu(inner.forward(x))),
            Ffn::SwiGlu { up, gate, down } => down.forward(up.forward(x.clone()) * activation::silu(gate.forward(x))),
        }
    }
}

struct Layer {
    q: Linear,
    k: Linear,
    v: Linear,
    attn_out: Linear,
    attn_norm: LayerNorm,
    ffn: Ffn,
    out_norm: LayerNorm,
}

/// A loaded encoder model.
pub struct Encoder {
    config: ModelConfig,
    device: Device,
    /// The embedding tables, row-major `[rows, hidden]`, on the host: a batch's rows are
    /// gathered and summed here and uploaded once. (The wgpu backend's `select` fails in
    /// Burn 0.22.1, and the gather is a copy either way.) `pos` is empty for rotary
    /// positions.
    word: Vec<f32>,
    pos: Vec<f32>,
    typ: Vec<f32>,
    emb_norm: LayerNorm,
    layers: Vec<Layer>,
}

struct Weights<'a> {
    st: safetensors::SafeTensors<'a>,
    device: &'a Device,
}

impl Weights<'_> {
    fn tensor<const D: usize>(&self, name: &str, shape: [usize; D]) -> Result<Tensor<D>, String> {
        Ok(self.upload(self.host(name, shape)?, shape))
    }

    fn upload<const D: usize>(&self, data: Vec<f32>, shape: [usize; D]) -> Tensor<D> {
        Tensor::from_data(TensorData::new(data, shape.to_vec()), self.device)
    }

    /// The weights `name`, checked to have `shape`, as host floats.
    fn host<const D: usize>(&self, name: &str, shape: [usize; D]) -> Result<Vec<f32>, String> {
        let view = ["", "bert.", "roberta."]
            .iter()
            .find_map(|p| self.st.tensor(&format!("{p}{name}")).ok())
            .ok_or_else(|| format!("the weights have no `{name}`"))?;
        if view.shape() != shape {
            return Err(format!("`{name}` has shape {:?}, expected {shape:?}", view.shape()));
        }
        // Half-precision weights are widened: the model runs in f32 either way.
        let data = view.data();
        Ok(match view.dtype() {
            safetensors::Dtype::F32 => data.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect(),
            safetensors::Dtype::F16 => data.as_chunks::<2>().0.iter().map(|c| f16::from_le_bytes(*c).to_f32()).collect(),
            safetensors::Dtype::BF16 => data.as_chunks::<2>().0.iter().map(|c| bf16::from_le_bytes(*c).to_f32()).collect(),
            other => return Err(format!("`{name}` is {other:?}; only F32, F16 and BF16 weights are supported")),
        })
    }

    fn bias(&self, name: &str, size: usize, has: bool) -> Result<Option<Tensor<1>>, String> {
        has.then(|| self.tensor(name, [size])).transpose()
    }

    fn linear(&self, prefix: &str, input: usize, output: usize, bias: bool) -> Result<Linear, String> {
        Ok(Linear {
            w: self.tensor(&format!("{prefix}.weight"), [output, input])?.transpose(),
            b: self.bias(&format!("{prefix}.bias"), output, bias)?,
        })
    }

    /// The `parts` equal projections that `prefix` stacks (nomic-bert's `Wqkv`: query,
    /// key and value), each `input` → `output`.
    fn split_linear(&self, prefix: &str, parts: usize, input: usize, output: usize, bias: bool) -> Result<Vec<Linear>, String> {
        let w = self.host(&format!("{prefix}.weight"), [parts * output, input])?;
        let b = match bias {
            true => Some(self.host(&format!("{prefix}.bias"), [parts * output])?),
            false => None,
        };
        Ok((0..parts)
            .map(|i| Linear {
                w: self.upload(w[i * output * input..][..output * input].to_vec(), [output, input]).transpose(),
                b: b.as_ref().map(|b| self.upload(b[i * output..][..output].to_vec(), [output])),
            })
            .collect())
    }

    fn norm(&self, prefix: &str, size: usize, eps: f64) -> Result<LayerNorm, String> {
        Ok(LayerNorm {
            w: self.tensor(&format!("{prefix}.weight"), [size])?,
            b: self.tensor(&format!("{prefix}.bias"), [size])?,
            eps,
        })
    }
}

/// The layers of a BERT or XLM-RoBERTa model.
fn bert_layers(w: &Weights<'_>, c: &ModelConfig) -> Result<Vec<Layer>, String> {
    let (h, ff, eps) = (c.hidden_size, c.intermediate_size, c.layer_norm_eps);
    (0..c.num_hidden_layers)
        .map(|i| {
            let p = format!("encoder.layer.{i}");
            Ok(Layer {
                q: w.linear(&format!("{p}.attention.self.query"), h, h, true)?,
                k: w.linear(&format!("{p}.attention.self.key"), h, h, true)?,
                v: w.linear(&format!("{p}.attention.self.value"), h, h, true)?,
                attn_out: w.linear(&format!("{p}.attention.output.dense"), h, h, true)?,
                attn_norm: w.norm(&format!("{p}.attention.output.LayerNorm"), h, eps)?,
                ffn: Ffn::Gelu {
                    inner: w.linear(&format!("{p}.intermediate.dense"), h, ff, true)?,
                    out: w.linear(&format!("{p}.output.dense"), ff, h, true)?,
                },
                out_norm: w.norm(&format!("{p}.output.LayerNorm"), h, eps)?,
            })
        })
        .collect()
}

/// The layers of a nomic-bert model.
fn nomic_layers(w: &Weights<'_>, c: &ModelConfig) -> Result<Vec<Layer>, String> {
    let (h, ff, eps) = (c.hidden_size, c.intermediate_size, c.layer_norm_eps);
    (0..c.num_hidden_layers)
        .map(|i| {
            let p = format!("encoder.layers.{i}");
            let mut qkv = w.split_linear(&format!("{p}.attn.Wqkv"), 3, h, h, c.attention_bias)?.into_iter();
            let (Some(q), Some(k), Some(v)) = (qkv.next(), qkv.next(), qkv.next()) else { unreachable!() };
            Ok(Layer {
                q,
                k,
                v,
                attn_out: w.linear(&format!("{p}.attn.out_proj"), h, h, c.attention_bias)?,
                attn_norm: w.norm(&format!("{p}.norm1"), h, eps)?,
                ffn: Ffn::SwiGlu {
                    up: w.linear(&format!("{p}.mlp.fc11"), h, ff, c.ffn_in_bias)?,
                    gate: w.linear(&format!("{p}.mlp.fc12"), h, ff, c.ffn_in_bias)?,
                    down: w.linear(&format!("{p}.mlp.fc2"), ff, h, c.ffn_out_bias)?,
                },
                out_norm: w.norm(&format!("{p}.norm2"), h, eps)?,
            })
        })
        .collect()
}

/// `cos` and `sin` of the rotary angles for positions `0..width`, `[1, 1, width, dims]`
/// with each half repeated (as GPT-NeoX and nomic-bert rotate the halves). In f32, as
/// PyTorch computes them.
fn rotary_tables(base: f64, width: usize, dims: usize, device: &Device) -> (Tensor<4>, Tensor<4>) {
    let half = dims / 2;
    let inv: Vec<f32> = (0..half).map(|j| 1.0 / (base as f32).powf((2 * j) as f32 / dims as f32)).collect();
    let (mut cos, mut sin) = (Vec::with_capacity(width * dims), Vec::with_capacity(width * dims));
    for p in 0..width {
        for j in 0..dims {
            let angle = p as f32 * inv[j % half];
            cos.push(angle.cos());
            sin.push(angle.sin());
        }
    }
    let t = |v: Vec<f32>| Tensor::<4>::from_data(TensorData::new(v, [1, 1, width, dims]), device);
    (t(cos), t(sin))
}

/// `x` (`[batch, heads, tokens, dims]`) rotated by the angles of `cos` and `sin`.
fn rotate(x: Tensor<4>, cos: &Tensor<4>, sin: &Tensor<4>) -> Tensor<4> {
    let half = x.dims()[3] / 2;
    let first = x.clone().narrow(3, 0, half);
    let second = x.clone().narrow(3, half, half);
    let turned = Tensor::cat(vec![second.neg(), first], 3);
    x * cos.clone() + turned * sin.clone()
}

impl Encoder {
    /// Loads `model.safetensors` from `file` onto `device`.
    pub fn load(file: &Path, config: &ModelConfig, device: &Device) -> Result<Encoder, String> {
        let bytes = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let st = safetensors::SafeTensors::deserialize(&bytes).map_err(|e| format!("{}: {e}", file.display()))?;
        let w = Weights { st, device };
        let c = config;
        let h = c.hidden_size;
        let (layers, emb_norm) = match c.architecture {
            Architecture::Bert | Architecture::XlmRoberta => (bert_layers(&w, c)?, w.norm("embeddings.LayerNorm", h, c.layer_norm_eps)?),
            Architecture::NomicBert => (nomic_layers(&w, c)?, w.norm("emb_ln", h, c.layer_norm_eps)?),
        };
        let pos = match c.positions {
            Positions::Learned { .. } => w.host("embeddings.position_embeddings.weight", [c.max_position_embeddings, h])?,
            Positions::Rotary { .. } => Vec::new(),
        };
        Ok(Encoder {
            word: w.host("embeddings.word_embeddings.weight", [c.vocab_size, h])?,
            pos,
            typ: w.host("embeddings.token_type_embeddings.weight", [c.type_vocab_size, h])?,
            emb_norm,
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
        let ffn = match c.feed_forward {
            FeedForward::Gelu => 2,
            FeedForward::SwiGlu => 3,
        };
        let per_token = 6 * c.hidden_size + ffn * c.intermediate_size + 2 * c.num_attention_heads * width;
        4usize.saturating_mul(rows).saturating_mul(width).saturating_mul(per_token)
    }

    /// The vectors of a padded batch, one per row.
    pub fn embed(&self, batch: &Padded, pooling: Pooling, normalize: bool) -> Result<Vec<Vec<f32>>, String> {
        let (b, t, h) = (batch.rows, batch.width, self.config.hidden_size);
        if t > self.config.max_tokens {
            return Err(format!("{t} tokens is more than the model's {}", self.config.max_tokens));
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
        // word + token type + position, in that order (as transformers adds them). With
        // padding only at the end, token i of a row is at position i (padding gets
        // positions too, but nothing attends to it or pools it).
        let offset = match self.config.positions {
            Positions::Learned { offset } => Some(offset),
            Positions::Rotary { .. } => None,
        };
        let mut x = vec![0f32; b * t * h];
        for (n, out) in x.chunks_exact_mut(h).enumerate() {
            let (id, ty) = (batch.ids[n] as usize, batch.type_ids[n] as usize);
            let (word, typ) = (&self.word[id * h..][..h], &self.typ[ty * h..][..h]);
            for k in 0..h {
                out[k] = word[k] + typ[k];
            }
            if let Some(offset) = offset {
                let pos = &self.pos[(offset + n % t) * h..][..h];
                for k in 0..h {
                    out[k] += pos[k];
                }
            }
        }
        let x = Tensor::<3>::from_data(TensorData::new(x, [b, t, h]), dev);
        let mut x = self.emb_norm.forward(x);

        let mask = Tensor::<2>::from_data(TensorData::new(batch.mask.clone(), [b, t]), dev);
        // Added to the attention scores: 0 for tokens, very negative for padding.
        let bias = (mask.clone().neg() + 1.0).mul_scalar(-1e9).reshape([b, 1, 1, t]);
        let scale = 1.0 / (hd as f64).sqrt();
        let rotary = match self.config.positions {
            Positions::Rotary { base } => Some(rotary_tables(base, t, hd, dev)),
            Positions::Learned { .. } => None,
        };
        let split = |y: Tensor<3>| y.reshape([b, t, heads, hd]).swap_dims(1, 2);
        for l in &self.layers {
            let mut q = split(l.q.forward(x.clone()));
            let mut k = split(l.k.forward(x.clone()));
            let v = split(l.v.forward(x.clone()));
            if let Some((cos, sin)) = &rotary {
                q = rotate(q, cos, sin);
                k = rotate(k, cos, sin);
            }
            let scores = q.matmul(k.transpose()).mul_scalar(scale) + bias.clone();
            let ctx = activation::softmax(scores, 3).matmul(v).swap_dims(1, 2).reshape([b, t, h]);
            x = l.attn_norm.forward(l.attn_out.forward(ctx) + x);
            let ff = l.ffn.forward(x.clone());
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
