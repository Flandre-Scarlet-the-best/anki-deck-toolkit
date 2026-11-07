//! Подготовка входа и вычисление эмиссий CTC.

use crate::encoder::Model;
use crate::model::ModelIdentity;

/// Эмиссии модели: `frames × vocab` логарифмов вероятностей по строкам.
#[derive(Debug, Clone, PartialEq)]
pub struct Emissions {
    pub frames: usize,
    pub vocab: usize,
    pub log_probs: Vec<f32>,
}

/// Дополняет вход тишиной по манифесту и при необходимости нормализует его
/// к нулевому среднему и единичной дисперсии (`eps = 1e-7`, как в
/// `Wav2Vec2FeatureExtractor`). Нормализация считается по всему
/// дополненному сигналу — так модель обучалась и так её вызывает upstream.
pub fn prepare_input(identity: &ModelIdentity, samples: &[f32]) -> Vec<f32> {
    let mut padded = vec![0.0f32; identity.lead_pad_samples];
    padded.extend_from_slice(samples);
    padded.resize(padded.len() + identity.trail_pad_samples, 0.0);
    if identity.normalize_input && !padded.is_empty() {
        let n = padded.len() as f64;
        let mean = padded.iter().map(|v| f64::from(*v)).sum::<f64>() / n;
        let var = padded
            .iter()
            .map(|v| (f64::from(*v) - mean).powi(2))
            .sum::<f64>()
            / n;
        let inv = 1.0 / (var + 1e-7).sqrt();
        for v in &mut padded {
            *v = ((f64::from(*v) - mean) * inv) as f32;
        }
    }
    padded
}

pub fn emissions(model: &Model, identity: &ModelIdentity, samples: &[f32]) -> Emissions {
    let input = prepare_input(identity, samples);
    let log_probs = model.log_probs(&input);
    let (frames, vocab) = log_probs.dim();
    Emissions {
        frames,
        vocab,
        log_probs: log_probs.into_iter().collect(),
    }
}
