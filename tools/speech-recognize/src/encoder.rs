//! Прямой проход CTC-моделей семейства wav2vec2/HuBERT.
//!
//! Поддерживаются два варианта архитектуры transformers:
//! - [`Architecture::PostLayerNorm`] — `feat_extract_norm = "group"`,
//!   `do_stable_layer_norm = false` (HuBERT base, wav2vec2 base);
//! - [`Architecture::StableLayerNorm`] — `feat_extract_norm = "layer"`,
//!   `do_stable_layer_norm = true` (wav2vec2 large / XLSR).
//!
//! Режим только вывода: dropout, layerdrop и маскирование отключены.

use std::collections::BTreeMap;

use ndarray::{Array2, ArrayView2};

use crate::nn;
use crate::safetensors::Tensor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Architecture {
    PostLayerNorm,
    StableLayerNorm,
}

/// Размерности модели. Для закреплённых моделей задаются манифестом и
/// сверяются с формами тензоров; синтетические тесты задают свои.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub architecture: Architecture,
    /// Префикс имён тензоров (`hubert` или `wav2vec2`).
    pub prefix: String,
    pub conv_dim: usize,
    pub conv_kernel: Vec<usize>,
    pub conv_stride: Vec<usize>,
    pub conv_bias: bool,
    pub hidden: usize,
    pub layers: usize,
    pub heads: usize,
    pub intermediate: usize,
    pub pos_kernel: usize,
    pub pos_groups: usize,
    pub vocab: usize,
}

impl Spec {
    /// Число выходных кадров для входа длины `samples`.
    pub fn frames_for(&self, samples: usize) -> usize {
        self.conv_kernel
            .iter()
            .zip(&self.conv_stride)
            .fold(samples, |len, (k, s)| nn::conv_out_len(len, *k, *s))
    }

    /// Шаг кадра в отсчётах входа (произведение шагов свёрток).
    pub fn frame_stride(&self) -> usize {
        self.conv_stride.iter().product()
    }

    /// Рецептивное поле кадра свёрточного стека в отсчётах входа.
    pub fn receptive_field(&self) -> usize {
        let mut field = 1;
        let mut jump = 1;
        for (k, s) in self.conv_kernel.iter().zip(&self.conv_stride) {
            field += (k - 1) * jump;
            jump *= s;
        }
        field
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildError(pub String);

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "веса не соответствуют архитектуре: {}", self.0)
    }
}

impl std::error::Error for BuildError {}

const LAYER_NORM_EPS: f64 = 1e-5;

struct Norm {
    weight: Vec<f32>,
    bias: Vec<f32>,
}

struct ConvLayer {
    weight: Array2<f32>,
    bias: Option<Vec<f32>>,
    norm: Option<Norm>,
    kernel: usize,
    stride: usize,
}

struct Dense {
    weight: Array2<f32>,
    bias: Vec<f32>,
}

struct Layer {
    q: Dense,
    k: Dense,
    v: Dense,
    out: Dense,
    attn_norm: Norm,
    ff_in: Dense,
    ff_out: Dense,
    final_norm: Norm,
}

pub struct Model {
    spec: Spec,
    convs: Vec<ConvLayer>,
    projection_norm: Norm,
    projection: Dense,
    pos_weight: Array2<f32>,
    pos_bias: Vec<f32>,
    encoder_norm: Norm,
    layers: Vec<Layer>,
    head: Dense,
}

struct Weights {
    tensors: BTreeMap<String, Tensor>,
    used: Vec<String>,
}

impl Weights {
    fn take(&mut self, name: &str, shape: &[usize]) -> Result<Vec<f32>, BuildError> {
        let tensor = self
            .tensors
            .remove(name)
            .ok_or_else(|| BuildError(format!("нет тензора {name}")))?;
        if tensor.shape != shape {
            return Err(BuildError(format!(
                "{name}: форма {:?} вместо {shape:?}",
                tensor.shape
            )));
        }
        self.used.push(name.to_string());
        Ok(tensor.data)
    }

    fn matrix(
        &mut self,
        name: &str,
        rows: usize,
        shape: &[usize],
    ) -> Result<Array2<f32>, BuildError> {
        let data = self.take(name, shape)?;
        let cols = data.len() / rows;
        Ok(Array2::from_shape_vec((rows, cols), data).expect("форма проверена"))
    }

    fn norm(&mut self, prefix: &str, dim: usize) -> Result<Norm, BuildError> {
        Ok(Norm {
            weight: self.take(&format!("{prefix}.weight"), &[dim])?,
            bias: self.take(&format!("{prefix}.bias"), &[dim])?,
        })
    }

    fn dense(&mut self, prefix: &str, out: usize, inp: usize) -> Result<Dense, BuildError> {
        Ok(Dense {
            weight: self.matrix(&format!("{prefix}.weight"), out, &[out, inp])?,
            bias: self.take(&format!("{prefix}.bias"), &[out])?,
        })
    }
}

/// Вес позиционной свёртки после weight norm по оси ядра (`dim=2`):
/// `w[o,i,k] = g[k] · v[o,i,k] / ‖v[:,:,k]‖`.
fn weight_norm_dim2(g: &[f32], v: &[f32], out: usize, inp: usize, kernel: usize) -> Array2<f32> {
    let mut norms = vec![0.0f64; kernel];
    for (idx, value) in v.iter().enumerate() {
        norms[idx % kernel] += f64::from(*value).powi(2);
    }
    let scale: Vec<f64> = norms
        .iter()
        .zip(g)
        .map(|(n, g)| f64::from(*g) / n.sqrt())
        .collect();
    let data: Vec<f32> = v
        .iter()
        .enumerate()
        .map(|(idx, value)| (f64::from(*value) * scale[idx % kernel]) as f32)
        .collect();
    Array2::from_shape_vec((out, inp * kernel), data).expect("форма проверена")
}

impl Model {
    /// Собирает модель из тензоров, строго сверяя имена и формы. Лишние
    /// тензоры, кроме не используемого при выводе `masked_spec_embed`, — отказ:
    /// чекпойнт другой архитектуры не должен молча загрузиться частично.
    pub fn build(spec: Spec, tensors: BTreeMap<String, Tensor>) -> Result<Self, BuildError> {
        if spec.conv_kernel.len() != spec.conv_stride.len() || spec.conv_kernel.is_empty() {
            return Err(BuildError("число ядер и шагов свёрток различается".into()));
        }
        if !spec.hidden.is_multiple_of(spec.heads) || !spec.hidden.is_multiple_of(spec.pos_groups) {
            return Err(BuildError(
                "размерности не делятся на число голов/групп".into(),
            ));
        }
        let mut w = Weights {
            tensors,
            used: Vec::new(),
        };
        let p = spec.prefix.clone();
        let mut convs = Vec::new();
        let mut channels = 1;
        for (i, (&kernel, &stride)) in spec.conv_kernel.iter().zip(&spec.conv_stride).enumerate() {
            let base = format!("{p}.feature_extractor.conv_layers.{i}");
            let weight = w.matrix(
                &format!("{base}.conv.weight"),
                spec.conv_dim,
                &[spec.conv_dim, channels, kernel],
            )?;
            let bias = if spec.conv_bias {
                Some(w.take(&format!("{base}.conv.bias"), &[spec.conv_dim])?)
            } else {
                None
            };
            let norm = match spec.architecture {
                Architecture::StableLayerNorm => {
                    Some(w.norm(&format!("{base}.layer_norm"), spec.conv_dim)?)
                }
                Architecture::PostLayerNorm if i == 0 => {
                    Some(w.norm(&format!("{base}.layer_norm"), spec.conv_dim)?)
                }
                Architecture::PostLayerNorm => None,
            };
            convs.push(ConvLayer {
                weight,
                bias,
                norm,
                kernel,
                stride,
            });
            channels = spec.conv_dim;
        }
        let projection_norm =
            w.norm(&format!("{p}.feature_projection.layer_norm"), spec.conv_dim)?;
        let projection = w.dense(
            &format!("{p}.feature_projection.projection"),
            spec.hidden,
            spec.conv_dim,
        )?;
        let pos = format!("{p}.encoder.pos_conv_embed.conv");
        let per_group = spec.hidden / spec.pos_groups;
        let v_shape = [spec.hidden, per_group, spec.pos_kernel];
        let g_shape = [1, 1, spec.pos_kernel];
        let (g, v) = if w.tensors.contains_key(&format!("{pos}.weight_g")) {
            (
                w.take(&format!("{pos}.weight_g"), &g_shape)?,
                w.take(&format!("{pos}.weight_v"), &v_shape)?,
            )
        } else {
            (
                w.take(
                    &format!("{pos}.parametrizations.weight.original0"),
                    &g_shape,
                )?,
                w.take(
                    &format!("{pos}.parametrizations.weight.original1"),
                    &v_shape,
                )?,
            )
        };
        let pos_weight = weight_norm_dim2(&g, &v, spec.hidden, per_group, spec.pos_kernel);
        let pos_bias = w.take(&format!("{pos}.bias"), &[spec.hidden])?;
        let encoder_norm = w.norm(&format!("{p}.encoder.layer_norm"), spec.hidden)?;
        let mut layers = Vec::new();
        for i in 0..spec.layers {
            let base = format!("{p}.encoder.layers.{i}");
            let (h, f) = (spec.hidden, spec.intermediate);
            layers.push(Layer {
                q: w.dense(&format!("{base}.attention.q_proj"), h, h)?,
                k: w.dense(&format!("{base}.attention.k_proj"), h, h)?,
                v: w.dense(&format!("{base}.attention.v_proj"), h, h)?,
                out: w.dense(&format!("{base}.attention.out_proj"), h, h)?,
                attn_norm: w.norm(&format!("{base}.layer_norm"), h)?,
                ff_in: w.dense(&format!("{base}.feed_forward.intermediate_dense"), f, h)?,
                ff_out: w.dense(&format!("{base}.feed_forward.output_dense"), h, f)?,
                final_norm: w.norm(&format!("{base}.final_layer_norm"), h)?,
            });
        }
        let head = w.dense("lm_head", spec.vocab, spec.hidden)?;
        w.tensors.remove(&format!("{p}.masked_spec_embed"));
        if let Some(extra) = w.tensors.keys().next() {
            return Err(BuildError(format!("лишний тензор {extra}")));
        }
        Ok(Self {
            spec,
            convs,
            projection_norm,
            projection,
            pos_weight,
            pos_bias,
            encoder_norm,
            layers,
            head,
        })
    }

    pub fn spec(&self) -> &Spec {
        &self.spec
    }

    /// Логиты `[кадры, словарь]` для моно-входа 16 kHz (уже дополненного и,
    /// если модель требует, нормализованного вызывающим кодом).
    pub fn logits(&self, samples: &[f32]) -> Array2<f32> {
        let frames = self.spec.frames_for(samples.len());
        if frames == 0 {
            return Array2::zeros((0, self.spec.vocab));
        }
        let mut x = Array2::from_shape_vec((1, samples.len()), samples.to_vec()).expect("форма");
        for (i, conv) in self.convs.iter().enumerate() {
            x = nn::conv1d(
                x.view(),
                &conv.weight,
                conv.bias.as_deref(),
                conv.kernel,
                conv.stride,
            );
            if let Some(norm) = &conv.norm {
                match self.spec.architecture {
                    Architecture::StableLayerNorm => {
                        nn::layer_norm_channels(&mut x, &norm.weight, &norm.bias, LAYER_NORM_EPS)
                    }
                    Architecture::PostLayerNorm => {
                        debug_assert_eq!(i, 0);
                        nn::group_norm_per_channel(&mut x, &norm.weight, &norm.bias, LAYER_NORM_EPS)
                    }
                }
            }
            nn::gelu_in_place(x.iter_mut());
        }
        let mut features = x.t().to_owned();
        nn::layer_norm_rows(
            &mut features,
            &self.projection_norm.weight,
            &self.projection_norm.bias,
            LAYER_NORM_EPS,
        );
        let mut hidden = nn::linear(
            features.view(),
            &self.projection.weight,
            Some(&self.projection.bias),
        );
        let pos = nn::positional_conv(
            hidden.view(),
            &self.pos_weight,
            &self.pos_bias,
            self.spec.pos_kernel,
            self.spec.pos_groups,
        );
        hidden += &pos;
        let stable = self.spec.architecture == Architecture::StableLayerNorm;
        if !stable {
            self.norm(&mut hidden, &self.encoder_norm);
        }
        for layer in &self.layers {
            hidden = if stable {
                self.stable_layer(layer, hidden.view())
            } else {
                self.post_layer(layer, hidden.view())
            };
        }
        if stable {
            self.norm(&mut hidden, &self.encoder_norm);
        }
        nn::linear(hidden.view(), &self.head.weight, Some(&self.head.bias))
    }

    /// Логарифмы апостериорных вероятностей `[кадры, словарь]`.
    pub fn log_probs(&self, samples: &[f32]) -> Array2<f32> {
        nn::log_softmax_rows(&self.logits(samples))
    }

    fn norm(&self, x: &mut Array2<f32>, norm: &Norm) {
        nn::layer_norm_rows(x, &norm.weight, &norm.bias, LAYER_NORM_EPS);
    }

    fn attention(&self, layer: &Layer, x: ArrayView2<'_, f32>) -> Array2<f32> {
        nn::Attention {
            q: (&layer.q.weight, &layer.q.bias),
            k: (&layer.k.weight, &layer.k.bias),
            v: (&layer.v.weight, &layer.v.bias),
            out: (&layer.out.weight, &layer.out.bias),
            heads: self.spec.heads,
        }
        .forward(x)
    }

    fn feed_forward(&self, layer: &Layer, x: ArrayView2<'_, f32>) -> Array2<f32> {
        let mut inner = nn::linear(x, &layer.ff_in.weight, Some(&layer.ff_in.bias));
        nn::gelu_in_place(inner.iter_mut());
        nn::linear(inner.view(), &layer.ff_out.weight, Some(&layer.ff_out.bias))
    }

    fn post_layer(&self, layer: &Layer, x: ArrayView2<'_, f32>) -> Array2<f32> {
        let mut h = self.attention(layer, x);
        h += &x;
        self.norm(&mut h, &layer.attn_norm);
        let ff = self.feed_forward(layer, h.view());
        h += &ff;
        self.norm(&mut h, &layer.final_norm);
        h
    }

    fn stable_layer(&self, layer: &Layer, x: ArrayView2<'_, f32>) -> Array2<f32> {
        let mut normed = x.to_owned();
        self.norm(&mut normed, &layer.attn_norm);
        let mut h = self.attention(layer, normed.view());
        h += &x;
        let mut normed = h.clone();
        self.norm(&mut normed, &layer.final_norm);
        let ff = self.feed_forward(layer, normed.view());
        h += &ff;
        h
    }
}
