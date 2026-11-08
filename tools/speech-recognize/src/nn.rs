//! Минимальные операции прямого прохода wav2vec2/HuBERT в f32.
//!
//! Матрицы активаций хранятся по строкам: свёрточный стек — `[каналы, время]`,
//! трансформер — `[время, признаки]`. Матричное умножение — безопасный API
//! `ndarray` поверх `matrixmultiply`; каждая выходная ячейка считается одним
//! потоком с фиксированным порядком суммирования, поэтому результат не
//! зависит от числа потоков. Статистики нормализаций накапливаются в f64.

use ndarray::{Array2, ArrayView2, Axis, Slice};

/// Столбцы `range` матрицы (без макроса `s!`, который разрешает `unsafe_code`).
fn columns<'a>(x: ArrayView2<'a, f32>, range: std::ops::Range<usize>) -> ArrayView2<'a, f32> {
    x.slice_axis_move(Axis(1), Slice::from(range))
}

/// Линейный слой `x · Wᵀ + b`, вес в раскладке PyTorch `[выход, вход]`.
pub fn linear(x: ArrayView2<'_, f32>, weight: &Array2<f32>, bias: Option<&[f32]>) -> Array2<f32> {
    let mut out = x.dot(&weight.t());
    if let Some(bias) = bias {
        for mut row in out.rows_mut() {
            for (value, b) in row.iter_mut().zip(bias) {
                *value += b;
            }
        }
    }
    out
}

/// Функция ошибок (Абрамовиц–Стиган 7.1.26 с поправкой знака): абсолютная
/// погрешность меньше 1.5e-7, что ниже точности f32 активаций.
fn erf(x: f64) -> f64 {
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs();
    let t = 1.0 / (1.0 + 0.327_591_1 * x);
    let poly = t
        * (0.254_829_592
            + t * (-0.284_496_736
                + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    sign * (1.0 - poly * (-x * x).exp())
}

/// Точная GELU (`hidden_act = "gelu"` в transformers): `x·Φ(x)`.
pub fn gelu_in_place<'a>(values: impl IntoIterator<Item = &'a mut f32>) {
    for v in values {
        let x = f64::from(*v);
        *v = (0.5 * x * (1.0 + erf(x / std::f64::consts::SQRT_2))) as f32;
    }
}

/// LayerNorm по последней оси `[n, d]`.
pub fn layer_norm_rows(x: &mut Array2<f32>, weight: &[f32], bias: &[f32], eps: f64) {
    for mut row in x.rows_mut() {
        let n = row.len() as f64;
        let mean = row.iter().map(|v| f64::from(*v)).sum::<f64>() / n;
        let var = row
            .iter()
            .map(|v| (f64::from(*v) - mean).powi(2))
            .sum::<f64>()
            / n;
        let inv = 1.0 / (var + eps).sqrt();
        for ((v, w), b) in row.iter_mut().zip(weight).zip(bias) {
            *v = ((f64::from(*v) - mean) * inv * f64::from(*w) + f64::from(*b)) as f32;
        }
    }
}

/// LayerNorm по каналам для раскладки `[каналы, время]` (свёрточный слой
/// wav2vec2 с `feat_extract_norm = "layer"`).
pub fn layer_norm_channels(x: &mut Array2<f32>, weight: &[f32], bias: &[f32], eps: f64) {
    let channels = x.nrows() as f64;
    for mut column in x.columns_mut() {
        let mean = column.iter().map(|v| f64::from(*v)).sum::<f64>() / channels;
        let var = column
            .iter()
            .map(|v| (f64::from(*v) - mean).powi(2))
            .sum::<f64>()
            / channels;
        let inv = 1.0 / (var + eps).sqrt();
        for ((v, w), b) in column.iter_mut().zip(weight).zip(bias) {
            *v = ((f64::from(*v) - mean) * inv * f64::from(*w) + f64::from(*b)) as f32;
        }
    }
}

/// GroupNorm с числом групп, равным числу каналов: каждый канал
/// нормализуется по времени (первый свёрточный слой HuBERT/wav2vec2-base).
pub fn group_norm_per_channel(x: &mut Array2<f32>, weight: &[f32], bias: &[f32], eps: f64) {
    for ((mut row, w), b) in x.rows_mut().into_iter().zip(weight).zip(bias) {
        let n = row.len() as f64;
        let mean = row.iter().map(|v| f64::from(*v)).sum::<f64>() / n;
        let var = row
            .iter()
            .map(|v| (f64::from(*v) - mean).powi(2))
            .sum::<f64>()
            / n;
        let inv = 1.0 / (var + eps).sqrt();
        for v in row.iter_mut() {
            *v = ((f64::from(*v) - mean) * inv * f64::from(*w) + f64::from(*b)) as f32;
        }
    }
}

/// Длина выхода свёртки без дополнения.
pub fn conv_out_len(len: usize, kernel: usize, stride: usize) -> usize {
    if len < kernel {
        0
    } else {
        (len - kernel) / stride + 1
    }
}

/// Свёртка `[C_in, T] → [C_out, T_out]` без дополнения через im2col. Вес в
/// раскладке PyTorch `[C_out, C_in, K]`, развёрнутый в `[C_out, C_in·K]`.
pub fn conv1d(
    x: ArrayView2<'_, f32>,
    weight: &Array2<f32>,
    bias: Option<&[f32]>,
    kernel: usize,
    stride: usize,
) -> Array2<f32> {
    let (c_in, len) = x.dim();
    let out_len = conv_out_len(len, kernel, stride);
    let mut cols = Array2::<f32>::zeros((out_len, c_in * kernel));
    for (t, mut row) in cols.rows_mut().into_iter().enumerate() {
        let start = t * stride;
        for c in 0..c_in {
            let src = x.row(c);
            row.slice_axis_mut(Axis(0), Slice::from(c * kernel..(c + 1) * kernel))
                .assign(&src.slice_axis(Axis(0), Slice::from(start..start + kernel)));
        }
    }
    let mut out = weight.dot(&cols.t());
    if let Some(bias) = bias {
        for (mut row, b) in out.rows_mut().into_iter().zip(bias) {
            row.mapv_inplace(|v| v + b);
        }
    }
    out
}

/// Позиционная групповая свёртка энкодера: `[T, H] → [T, H]`, ядро `K`,
/// дополнение `K/2` с обеих сторон, последний кадр отбрасывается (чётное
/// ядро, `SamePad`), затем GELU. Вес — `[H, H/groups, K]` → `[H, H/groups·K]`.
pub fn positional_conv(
    x: ArrayView2<'_, f32>,
    weight: &Array2<f32>,
    bias: &[f32],
    kernel: usize,
    groups: usize,
) -> Array2<f32> {
    let (len, hidden) = x.dim();
    let per_group = hidden / groups;
    let pad = kernel / 2;
    let mut padded = Array2::<f32>::zeros((hidden, len + 2 * pad));
    padded
        .slice_axis_mut(Axis(1), Slice::from(pad..pad + len))
        .assign(&x.t());
    let mut out = Array2::<f32>::zeros((len, hidden));
    for g in 0..groups {
        let channels = g * per_group..(g + 1) * per_group;
        let input = padded.slice_axis(Axis(0), Slice::from(channels.clone()));
        let mut cols = Array2::<f32>::zeros((len, per_group * kernel));
        for (t, mut row) in cols.rows_mut().into_iter().enumerate() {
            for c in 0..per_group {
                row.slice_axis_mut(Axis(0), Slice::from(c * kernel..(c + 1) * kernel))
                    .assign(&input.row(c).slice_axis(Axis(0), Slice::from(t..t + kernel)));
            }
        }
        let w = weight.slice_axis(Axis(0), Slice::from(channels.clone()));
        let block = cols.dot(&w.t());
        out.slice_axis_mut(Axis(1), Slice::from(channels))
            .assign(&block);
    }
    for mut row in out.rows_mut() {
        for (v, b) in row.iter_mut().zip(bias) {
            *v += b;
        }
    }
    gelu_in_place(out.iter_mut());
    out
}

/// Многоголовое внимание без маски: вход `[T, H]`, проекции в раскладке
/// PyTorch. Масштаб применяется к запросам, как в transformers.
pub struct Attention<'a> {
    pub q: (&'a Array2<f32>, &'a [f32]),
    pub k: (&'a Array2<f32>, &'a [f32]),
    pub v: (&'a Array2<f32>, &'a [f32]),
    pub out: (&'a Array2<f32>, &'a [f32]),
    pub heads: usize,
}

impl Attention<'_> {
    pub fn forward(&self, x: ArrayView2<'_, f32>) -> Array2<f32> {
        let hidden = x.ncols();
        let head_dim = hidden / self.heads;
        let scale = (head_dim as f32).powf(-0.5);
        let mut q = linear(x, self.q.0, Some(self.q.1));
        q.mapv_inplace(|v| v * scale);
        let k = linear(x, self.k.0, Some(self.k.1));
        let v = linear(x, self.v.0, Some(self.v.1));
        let mut context = Array2::<f32>::zeros(x.dim());
        for h in 0..self.heads {
            let cols = h * head_dim..(h + 1) * head_dim;
            let qh = columns(q.view(), cols.clone());
            let kh = columns(k.view(), cols.clone());
            let vh = columns(v.view(), cols.clone());
            let mut scores = qh.dot(&kh.t());
            softmax_rows(&mut scores);
            context
                .slice_axis_mut(Axis(1), Slice::from(cols))
                .assign(&scores.dot(&vh));
        }
        linear(context.view(), self.out.0, Some(self.out.1))
    }
}

pub fn softmax_rows(x: &mut Array2<f32>) {
    for mut row in x.rows_mut() {
        let max = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let mut sum = 0.0f64;
        for v in row.iter_mut() {
            let e = f64::from(*v - max).exp();
            *v = e as f32;
            sum += e;
        }
        let inv = (1.0 / sum) as f32;
        row.mapv_inplace(|v| v * inv);
    }
}

/// Логарифм softmax по словарю для каждого кадра.
pub fn log_softmax_rows(x: &Array2<f32>) -> Array2<f32> {
    let mut out = x.clone();
    for mut row in out.rows_mut() {
        let max = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let sum: f64 = row.iter().map(|v| f64::from(*v - max).exp()).sum();
        let log_sum = sum.ln() as f32 + max;
        row.mapv_inplace(|v| v - log_sum);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn erf_matches_reference_values() {
        for (x, expected) in [
            (0.0, 0.0),
            (0.5, 0.520_499_877_8),
            (1.0, 0.842_700_792_9),
            (2.0, 0.995_322_265_0),
            (-1.0, -0.842_700_792_9),
        ] {
            assert!((erf(x) - expected).abs() < 2e-7, "erf({x})");
        }
    }

    #[test]
    fn conv1d_matches_direct_definition() {
        let x = Array2::from_shape_fn((2, 9), |(c, t)| (c * 10 + t) as f32 * 0.1);
        let w = Array2::from_shape_fn((3, 2 * 3), |(o, i)| {
            (o as f32 - 1.0) * 0.5 + i as f32 * 0.01
        });
        let out = conv1d(x.view(), &w, Some(&[0.1, 0.2, 0.3]), 3, 2);
        assert_eq!(out.dim(), (3, 4));
        for o in 0..3 {
            for t in 0..4 {
                let mut acc = [0.1, 0.2, 0.3][o];
                for c in 0..2 {
                    for k in 0..3 {
                        acc += w[[o, c * 3 + k]] * x[[c, t * 2 + k]];
                    }
                }
                assert!((out[[o, t]] - acc).abs() < 1e-5);
            }
        }
    }

    #[test]
    fn positional_conv_keeps_length_and_matches_definition() {
        let (len, hidden, kernel, groups) = (5, 4, 4, 2);
        let x = Array2::from_shape_fn((len, hidden), |(t, h)| ((t * 3 + h) % 7) as f32 - 3.0);
        let per = hidden / groups;
        let w = Array2::from_shape_fn((hidden, per * kernel), |(o, i)| {
            ((o * 5 + i) % 11) as f32 * 0.05 - 0.2
        });
        let bias = [0.0, 0.1, -0.1, 0.2];
        let out = positional_conv(x.view(), &w, &bias, kernel, groups);
        assert_eq!(out.dim(), (len, hidden));
        let pad = kernel / 2;
        for t in 0..len {
            for o in 0..hidden {
                let g = o / per;
                let mut acc = bias[o];
                for c in 0..per {
                    for k in 0..kernel {
                        let src = t as isize + k as isize - pad as isize;
                        if (0..len as isize).contains(&src) {
                            acc += w[[o, c * kernel + k]] * x[[src as usize, g * per + c]];
                        }
                    }
                }
                let mut expected = [acc];
                gelu_in_place(expected.iter_mut());
                assert!((out[[t, o]] - expected[0]).abs() < 1e-5, "t={t} o={o}");
            }
        }
    }

    #[test]
    fn log_softmax_rows_normalise() {
        let x = Array2::from_shape_vec((2, 3), vec![1.0, 2.0, 3.0, -50.0, 0.0, 50.0]).unwrap();
        let out = log_softmax_rows(&x);
        for row in out.rows() {
            let total: f64 = row.iter().map(|v| f64::from(*v).exp()).sum();
            assert!((total - 1.0).abs() < 1e-6);
        }
    }
}
