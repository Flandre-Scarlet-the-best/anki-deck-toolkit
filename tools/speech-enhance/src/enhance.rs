//! Офлайн-применение потоковой DeepFilterNet3 к короткому клипу.
//!
//! Модель потоковая: кадры по `hop` отсчётов, выход отстаёт от входа на
//! [`DfTract::delay`] отсчётов (перекрытие STFT и lookahead). Клип
//! обрабатывается целиком: вход дополняется нулями на задержку и до целого
//! кадра (сброс хвоста), из выхода удаляются первые `delay` отсчётов, и
//! результат обрезается до исходной длины. Поэтому длина выхода равна длине
//! входа, а задержка компенсирована точно, без добавленной тишины и без
//! потери конца клипа.
//!
//! Состояние модели между клипами не переносится: каждый канал каждого
//! клипа обрабатывается собственным клоном только что созданного
//! экземпляра. Каналы обрабатываются независимо — сведения в моно нет,
//! поэтому противофазное стерео не гасится.

use deep_filter_rt::tract::{DfParams, DfTract, RuntimeParams};
use ndarray::Array2;
use serde::{Deserialize, Serialize};

use crate::model::{ModelFiles, SAMPLE_RATE};

/// Параметры инференса; входят в идентичность результата.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceParams {
    /// Предел ослабления модели в dB (100 и больше — без предела).
    pub atten_lim_db: f32,
    /// Коэффициент постфильтра; 0 — постфильтр выключен.
    pub post_filter_beta: f32,
    /// Контекст прогрева на каждом краю, мс: зеркальное продолжение клипа
    /// перед началом и после конца. Потоковая модель стартует с нулевых
    /// состояний нормировки и рекуррентных слоёв; без контекста первые
    /// кадры звука подавляются как «начало потока». Контекст обрабатывается
    /// и отбрасывается, длина и выравнивание результата не меняются.
    #[serde(default)]
    pub context_ms: u32,
}

impl Default for InferenceParams {
    fn default() -> Self {
        Self {
            atten_lim_db: 100.0,
            post_filter_beta: 0.0,
            context_ms: 0,
        }
    }
}

/// Индекс зеркального («пинг-понг») продолжения последовательности длины
/// `len` для позиции `i` (может быть отрицательной или за концом).
fn mirror(i: i64, len: usize) -> usize {
    if len == 1 {
        return 0;
    }
    let period = 2 * (len as i64 - 1);
    let k = i.rem_euclid(period);
    (if k < len as i64 { k } else { period - k }) as usize
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelStats {
    pub frames: u64,
    /// Оценка локального SNR моделью по кадрам (dB).
    pub lsnr_min_db: f32,
    pub lsnr_mean_db: f32,
    pub lsnr_max_db: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnostics {
    pub sample_rate: u32,
    pub channels: u32,
    pub input_frames: u64,
    pub output_frames: u64,
    /// Компенсированная задержка модели в отсчётах.
    pub delay_samples: u64,
    pub hop_samples: u64,
    /// Дополнение нулями в конце (задержка + до целого кадра).
    pub flush_samples: u64,
    /// Зеркальный контекст прогрева на каждом краю, отсчётов.
    pub context_samples: u64,
    /// Каналы побайтно совпали: модель выполнена один раз, результат
    /// скопирован (эквивалентно независимой обработке каждого канала).
    pub identical_channels: bool,
    pub per_channel: Vec<ChannelStats>,
}

#[derive(Debug)]
pub struct EnhanceError(pub String);

impl std::fmt::Display for EnhanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EnhanceError {}

/// Подготовленная модель: шаблон, клонируемый для каждого канала клипа.
pub struct Enhancer {
    template: DfTract,
    context: usize,
}

impl Enhancer {
    pub fn new(files: &ModelFiles, params: &InferenceParams) -> Result<Self, EnhanceError> {
        if !(params.atten_lim_db.is_finite() && params.atten_lim_db >= 0.0)
            || !(params.post_filter_beta.is_finite() && params.post_filter_beta >= 0.0)
        {
            return Err(EnhanceError(
                "параметры инференса должны быть конечными и неотрицательными".into(),
            ));
        }
        let dfp = DfParams::from_bytes(
            &files.config,
            files.enc.clone(),
            files.erb_dec.clone(),
            files.df_dec.clone(),
        )
        .map_err(|e| EnhanceError(format!("конфигурация модели: {e:#}")))?;
        let rp = RuntimeParams::default_with_ch(1)
            .with_atten_lim(params.atten_lim_db)
            .with_post_filter(params.post_filter_beta);
        let template =
            DfTract::new(dfp, &rp).map_err(|e| EnhanceError(format!("загрузка модели: {e:#}")))?;
        if template.sr != SAMPLE_RATE as usize {
            return Err(EnhanceError(format!(
                "модель работает на {} Hz, ожидалось {SAMPLE_RATE}",
                template.sr
            )));
        }
        let context = (u64::from(params.context_ms) * u64::from(SAMPLE_RATE) / 1000) as usize;
        Ok(Self { template, context })
    }

    pub fn delay(&self) -> usize {
        self.template.delay()
    }

    pub fn hop(&self) -> usize {
        self.template.hop_size
    }

    fn channel(&self, input: &[f32]) -> Result<(Vec<f32>, ChannelStats), EnhanceError> {
        let mut df = self.template.clone();
        let hop = df.hop_size;
        let delay = df.delay();
        let ctx = self.context;
        let extended: Vec<f32> = (-(ctx as i64)..(input.len() + ctx) as i64)
            .map(|i| input[mirror(i, input.len())])
            .collect();
        let padded_len = (extended.len() + delay).div_ceil(hop) * hop;
        let mut padded = extended;
        padded.resize(padded_len, 0.0);
        let mut output = Vec::with_capacity(padded_len);
        let mut enh = Array2::<f32>::zeros((1, hop));
        let (mut min, mut max, mut sum) = (f32::INFINITY, f32::NEG_INFINITY, 0f64);
        let mut frames = 0u64;
        for frame in padded.chunks_exact(hop) {
            let noisy =
                Array2::from_shape_vec((1, hop), frame.to_vec()).expect("форма кадра согласована");
            let lsnr = df
                .process(noisy.view(), enh.view_mut())
                .map_err(|e| EnhanceError(format!("инференс: {e:#}")))?;
            min = min.min(lsnr);
            max = max.max(lsnr);
            sum += f64::from(lsnr);
            frames += 1;
            output.extend(enh.iter());
        }
        let result: Vec<f32> = output[delay + ctx..delay + ctx + input.len()].to_vec();
        if result.iter().any(|s| !s.is_finite()) {
            return Err(EnhanceError("модель выдала неконечные отсчёты".into()));
        }
        Ok((
            result,
            ChannelStats {
                frames,
                lsnr_min_db: if frames > 0 { min } else { 0.0 },
                lsnr_mean_db: if frames > 0 {
                    (sum / frames as f64) as f32
                } else {
                    0.0
                },
                lsnr_max_db: if frames > 0 { max } else { 0.0 },
            },
        ))
    }

    /// Улучшает клип 48 kHz с чередованием `channels` каналов. Длина
    /// результата равна длине входа.
    pub fn enhance(
        &self,
        channels: usize,
        interleaved: &[f32],
    ) -> Result<(Vec<f32>, Diagnostics), EnhanceError> {
        if channels == 0 || channels > 2 {
            return Err(EnhanceError("поддерживаются 1 или 2 канала".into()));
        }
        if interleaved.is_empty() || !interleaved.len().is_multiple_of(channels) {
            return Err(EnhanceError("пустой или неполный поток PCM".into()));
        }
        if interleaved.iter().any(|s| !s.is_finite()) {
            return Err(EnhanceError("вход содержит неконечные отсчёты".into()));
        }
        let frames = interleaved.len() / channels;
        let split: Vec<Vec<f32>> = (0..channels)
            .map(|c| {
                interleaved
                    .iter()
                    .skip(c)
                    .step_by(channels)
                    .copied()
                    .collect()
            })
            .collect();
        let identical = split.iter().all(|c| c == &split[0]);
        let mut outputs = Vec::with_capacity(channels);
        let mut stats = Vec::with_capacity(channels);
        if identical {
            let (out, s) = self.channel(&split[0])?;
            for _ in 0..channels {
                outputs.push(out.clone());
                stats.push(s.clone());
            }
        } else {
            for c in &split {
                let (out, s) = self.channel(c)?;
                outputs.push(out);
                stats.push(s);
            }
        }
        let mut result = Vec::with_capacity(interleaved.len());
        for i in 0..frames {
            for out in &outputs {
                result.push(out[i]);
            }
        }
        let delay = self.delay();
        let hop = self.hop();
        let extended = frames + 2 * self.context;
        let flush = (extended + delay).div_ceil(hop) * hop - extended;
        Ok((
            result,
            Diagnostics {
                sample_rate: SAMPLE_RATE,
                channels: channels as u32,
                input_frames: frames as u64,
                output_frames: frames as u64,
                delay_samples: delay as u64,
                hop_samples: hop as u64,
                flush_samples: flush as u64,
                context_samples: self.context as u64,
                identical_channels: identical,
                per_channel: stats,
            },
        ))
    }
}
