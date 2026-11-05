//! Детерминированные акустические измерения декодированного PCM.
//!
//! Модуль не запускает внешних процессов и не знает о файлах: он получает
//! interleaved `f32` и возвращает конечные числа либо `None`, если величина
//! для сигнала не определена. Ни одна функция не возвращает NaN/бесконечность.
//! Эвристики описывают уровень сигнала, а не японскую речь.

/// Нижняя граница уровней: цифровая тишина представляется этим числом, а не `-inf`.
pub const LEVEL_FLOOR_DB: f64 = -120.0;

/// Interleaved PCM с исходными частотой и числом каналов.
#[derive(Debug, Clone, PartialEq)]
pub struct Pcm {
    pub sample_rate: u32,
    pub channels: usize,
    pub samples: Vec<f32>,
}

impl Pcm {
    pub fn frames(&self) -> usize {
        self.samples.len().checked_div(self.channels).unwrap_or(0)
    }

    pub fn duration_seconds(&self) -> f64 {
        self.frames() as f64 / f64::from(self.sample_rate)
    }

    pub fn samples_for_ms(&self, ms: f64) -> usize {
        (f64::from(self.sample_rate) * ms / 1000.0).round().max(0.0) as usize
    }

    /// Срез по кадрам (а не по отдельным отсчётам) с сохранением каналов.
    pub fn slice_frames(&self, start: usize, end: usize) -> Pcm {
        let end = end.min(self.frames());
        let start = start.min(end);
        Pcm {
            sample_rate: self.sample_rate,
            channels: self.channels,
            samples: self.samples[start * self.channels..end * self.channels].to_vec(),
        }
    }
}

/// Амплитуда → dBFS с нижней границей вместо `-inf`.
pub fn amplitude_db(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        (20.0 * value.log10()).max(LEVEL_FLOOR_DB)
    } else {
        LEVEL_FLOOR_DB
    }
}

/// Средний квадрат → dB с нижней границей.
pub fn power_db(mean_square: f64) -> f64 {
    if mean_square.is_finite() && mean_square > 0.0 {
        (10.0 * mean_square.log10()).max(LEVEL_FLOOR_DB)
    } else {
        LEVEL_FLOOR_DB
    }
}

pub fn db_to_gain(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

#[derive(Debug, Clone, Copy)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 3],
}

impl Biquad {
    fn run(&self, input: impl Iterator<Item = f64>, mut sink: impl FnMut(usize, f64)) {
        let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
        for (index, x) in input.enumerate() {
            let y =
                self.b[0] * x + self.b[1] * x1 + self.b[2] * x2 - self.a[1] * y1 - self.a[2] * y2;
            x2 = x1;
            x1 = x;
            y2 = y1;
            y1 = y;
            sink(index, y);
        }
    }
}

/// Коэффициенты K-фильтра ITU-R BS.1770 для произвольной частоты дискретизации
/// (та же аналоговая модель, что в libebur128): high shelf и high-pass RLB.
fn k_weighting(sample_rate: u32) -> [Biquad; 2] {
    let fs = f64::from(sample_rate);
    let f0 = 1_681.974_450_955_533;
    let gain = 3.999_843_853_973_347;
    let q = 0.707_175_236_955_419_6;
    let k = (std::f64::consts::PI * f0 / fs).tan();
    let vh = 10f64.powf(gain / 20.0);
    let vb = vh.powf(0.499_666_774_154_541_6);
    let a0 = 1.0 + k / q + k * k;
    let shelf = Biquad {
        b: [
            (vh + vb * k / q + k * k) / a0,
            2.0 * (k * k - vh) / a0,
            (vh - vb * k / q + k * k) / a0,
        ],
        a: [1.0, 2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
    };
    let f0 = 38.135_470_876_024_44;
    let q = 0.500_327_037_323_877_3;
    let k = (std::f64::consts::PI * f0 / fs).tan();
    let a0 = 1.0 + k / q + k * k;
    let high_pass = Biquad {
        b: [1.0, -2.0, 1.0],
        a: [1.0, 2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
    };
    [shelf, high_pass]
}

/// Префиксные суммы K-взвешенных квадратов, сложенных по каналам с весом 1
/// (моно и стерео; объёмные раскладки MP3 не поддерживает).
fn kweighted_prefix(pcm: &Pcm) -> Vec<f64> {
    let frames = pcm.frames();
    let mut squares = vec![0.0f64; frames];
    let [shelf, high_pass] = k_weighting(pcm.sample_rate);
    for channel in 0..pcm.channels {
        let mut stage = vec![0.0f64; frames];
        shelf.run(
            (0..frames).map(|i| f64::from(pcm.samples[i * pcm.channels + channel])),
            |i, y| stage[i] = y,
        );
        high_pass.run(stage.iter().copied(), |i, y| squares[i] += y * y);
    }
    let mut prefix = Vec::with_capacity(frames + 1);
    prefix.push(0.0);
    let mut sum = 0.0;
    for value in squares {
        sum += value;
        prefix.push(sum);
    }
    prefix
}

/// Результат стробируемого измерения громкости.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GatedLoudness {
    pub lufs: f64,
    pub blocks_total: usize,
    pub blocks_gated: usize,
}

fn gated_loudness(prefix: &[f64], block: usize, step: usize) -> Option<GatedLoudness> {
    let frames = prefix.len().saturating_sub(1);
    if block == 0 || step == 0 || frames < block {
        return None;
    }
    let mut energies = Vec::new();
    let mut start = 0;
    while start + block <= frames {
        energies.push((prefix[start + block] - prefix[start]).max(0.0) / block as f64);
        start += step;
    }
    let loudness = |z: f64| -0.691 + power_db(z);
    let absolute: Vec<f64> = energies
        .iter()
        .copied()
        .filter(|&z| z > 0.0 && loudness(z) > -70.0)
        .collect();
    if absolute.is_empty() {
        return None;
    }
    let relative_gate = loudness(absolute.iter().sum::<f64>() / absolute.len() as f64) - 10.0;
    let gated: Vec<f64> = absolute
        .into_iter()
        .filter(|&z| loudness(z) > relative_gate)
        .collect();
    if gated.is_empty() {
        return None;
    }
    let lufs = loudness(gated.iter().sum::<f64>() / gated.len() as f64);
    lufs.is_finite().then_some(GatedLoudness {
        lufs,
        blocks_total: energies.len(),
        blocks_gated: gated.len(),
    })
}

/// Громкость по BS.1770: интегральная (блоки 400 мс, шаг 100 мс) и
/// альтернативная коротким окном (100 мс, шаг 25 мс) с теми же порогами.
/// Альтернатива нужна для коротких слов, у которых нет ни одного блока 400 мс.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Loudness {
    pub integrated: Option<GatedLoudness>,
    pub short_window: Option<GatedLoudness>,
}

pub fn loudness(pcm: &Pcm) -> Loudness {
    let prefix = kweighted_prefix(pcm);
    let rate = f64::from(pcm.sample_rate);
    let size = |seconds: f64| (rate * seconds).round() as usize;
    Loudness {
        integrated: gated_loudness(&prefix, size(0.4), size(0.1)),
        short_window: gated_loudness(&prefix, size(0.1), size(0.025)),
    }
}

/// Полифазный FIR 4× (windowed sinc с окном Блэкмана, 16 отводов на фазу).
/// Это та же схема передискретизации, что рекомендует BS.1770-4 для true peak;
/// точность — доли dB, а не эталон измерителя.
fn true_peak_filter() -> [[f64; 16]; 4] {
    const PHASES: usize = 4;
    const TAPS: usize = 16;
    let total = PHASES * TAPS;
    let centre = (total as f64 - 1.0) / 2.0;
    let mut phases = [[0.0; TAPS]; PHASES];
    for (index, phase) in (0..total).map(|i| (i, i % PHASES)) {
        let x = (index as f64 - centre) / PHASES as f64;
        let sinc = if x.abs() < 1e-12 {
            1.0
        } else {
            (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
        };
        let n = index as f64 / (total as f64 - 1.0);
        let window = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * n).cos()
            + 0.08 * (4.0 * std::f64::consts::PI * n).cos();
        phases[phase][index / PHASES] = sinc * window;
    }
    for phase in &mut phases {
        let sum: f64 = phase.iter().sum();
        for tap in phase.iter_mut() {
            *tap /= sum;
        }
    }
    phases
}

/// Максимум модуля после передискретизации 4× по всем каналам; не меньше пика отсчётов.
pub fn true_peak(pcm: &Pcm) -> f64 {
    let filter = true_peak_filter();
    let frames = pcm.frames();
    let mut peak = sample_peak(pcm);
    for channel in 0..pcm.channels {
        let at = |i: isize| -> f64 {
            if i < 0 || i as usize >= frames {
                0.0
            } else {
                f64::from(pcm.samples[i as usize * pcm.channels + channel])
            }
        };
        // Хвост в 16 нулевых отсчётов захватывает выброс после последнего кадра.
        for n in 0..frames as isize + 16 {
            for phase in &filter {
                let mut value = 0.0;
                for (tap, coefficient) in phase.iter().enumerate() {
                    value += coefficient * at(n - tap as isize);
                }
                peak = peak.max(value.abs());
            }
        }
    }
    peak
}

pub fn sample_peak(pcm: &Pcm) -> f64 {
    pcm.samples
        .iter()
        .map(|s| f64::from(s.abs()))
        .filter(|s| s.is_finite())
        .fold(0.0, f64::max)
}

pub fn rms(pcm: &Pcm) -> f64 {
    if pcm.samples.is_empty() {
        return 0.0;
    }
    (pcm.samples
        .iter()
        .map(|s| f64::from(*s).powi(2))
        .sum::<f64>()
        / pcm.samples.len() as f64)
        .sqrt()
}

/// Уровни последовательных неперекрывающихся окон: максимум RMS по каналам, dBFS.
/// Последнее неполное окно учитывается по фактической длине.
pub fn frame_levels(pcm: &Pcm, frame: usize) -> Vec<f64> {
    let frames = pcm.frames();
    if frame == 0 {
        return Vec::new();
    }
    (0..frames.div_ceil(frame))
        .map(|index| {
            let start = index * frame;
            let end = (start + frame).min(frames);
            (0..pcm.channels)
                .map(|channel| {
                    let sum: f64 = (start..end)
                        .map(|i| f64::from(pcm.samples[i * pcm.channels + channel]).powi(2))
                        .sum();
                    power_db(sum / (end - start) as f64)
                })
                .fold(LEVEL_FLOOR_DB, f64::max)
        })
        .collect()
}

/// Признаки перегрузки: отсчёты у полной шкалы, выше неё и серии подряд.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Clipping {
    pub near_full_scale_samples: u64,
    pub over_full_scale_samples: u64,
    /// Серии не короче трёх соседних отсчётов канала у полной шкалы.
    pub clip_runs: u64,
}

pub fn clipping(pcm: &Pcm) -> Clipping {
    let mut result = Clipping::default();
    for channel in 0..pcm.channels {
        let mut run = 0u64;
        for frame in 0..pcm.frames() {
            let value = pcm.samples[frame * pcm.channels + channel].abs();
            if value > 1.0 {
                result.over_full_scale_samples += 1;
            }
            if value >= 0.999 {
                result.near_full_scale_samples += 1;
                run += 1;
                if run == 3 {
                    result.clip_runs += 1;
                }
            } else {
                run = 0;
            }
        }
    }
    result
}

/// Среднее по каналам (постоянная составляющая), наибольшее по модулю.
pub fn dc_offset(pcm: &Pcm) -> f64 {
    let frames = pcm.frames();
    if frames == 0 {
        return 0.0;
    }
    (0..pcm.channels)
        .map(|channel| {
            (0..frames)
                .map(|i| f64::from(pcm.samples[i * pcm.channels + channel]))
                .sum::<f64>()
                / frames as f64
        })
        .fold(
            0.0f64,
            |acc, value| if value.abs() > acc.abs() { value } else { acc },
        )
}

/// Различие каналов стерео: уровень разности L−R относительно уровня сигнала, dB.
/// `None` для моно. Очень низкое значение означает фактически моно-запись.
pub fn stereo_difference_db(pcm: &Pcm) -> Option<f64> {
    if pcm.channels != 2 || pcm.frames() == 0 {
        return None;
    }
    let (mut diff, mut total) = (0.0f64, 0.0f64);
    for [left, right] in pcm.samples.as_chunks::<2>().0 {
        let (l, r) = (f64::from(*left), f64::from(*right));
        diff += (l - r).powi(2);
        total += (l * l + r * r) / 2.0;
    }
    if total <= 0.0 {
        return None;
    }
    Some(power_db(diff / total).max(LEVEL_FLOOR_DB))
}

/// Перцентиль ближайшего ранга по уже конечным значениям.
pub fn percentile(values: &[f64], fraction: f64) -> Option<f64> {
    let mut sorted: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    if sorted.is_empty() {
        return None;
    }
    sorted.sort_by(f64::total_cmp);
    let rank = ((fraction.clamp(0.0, 1.0) * sorted.len() as f64).ceil() as usize).max(1);
    Some(sorted[rank - 1])
}

/// Линейное усиление и краевые фейды без какой-либо динамической обработки.
/// `fade_in`/`fade_out` — длины полукосинусных фейдов в кадрах.
pub fn apply_gain_and_fades(pcm: &mut Pcm, gain: f64, fade_in: usize, fade_out: usize) {
    let frames = pcm.frames();
    let fade = |position: usize, length: usize| -> f64 {
        0.5 - 0.5 * (std::f64::consts::PI * (position as f64 + 0.5) / length as f64).cos()
    };
    for frame in 0..frames {
        let mut factor = gain;
        if fade_in > 0 && frame < fade_in {
            factor *= fade(frame, fade_in);
        }
        if fade_out > 0 && frame + fade_out >= frames {
            factor *= fade(frames - 1 - frame, fade_out);
        }
        for channel in 0..pcm.channels {
            let index = frame * pcm.channels + channel;
            pcm.samples[index] = (f64::from(pcm.samples[index]) * factor) as f32;
        }
    }
}

/// Отношение сигнал/ошибка между эталоном и проверяемым сигналом одинаковой
/// раскладки с подбором целочисленного сдвига в пределах `max_lag` кадров.
/// Сдвиг ищется по окну наибольшей энергии, чтобы не перемножать весь файл.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Alignment {
    pub lag_frames: i64,
    pub snr_db: f64,
}

pub fn aligned_snr(reference: &Pcm, test: &Pcm, max_lag: usize) -> Option<Alignment> {
    if reference.channels != test.channels
        || reference.sample_rate != test.sample_rate
        || reference.frames() == 0
        || test.frames() == 0
    {
        return None;
    }
    let channels = reference.channels;
    let mono = |pcm: &Pcm| -> Vec<f64> {
        pcm.samples
            .chunks_exact(channels)
            .map(|frame| frame.iter().map(|s| f64::from(*s)).sum::<f64>() / channels as f64)
            .collect()
    };
    let (r, t) = (mono(reference), mono(test));
    let window = 4096.min(r.len());
    let mut best_start = 0;
    let mut best_energy = -1.0;
    let mut energy: f64 = r[..window].iter().map(|v| v * v).sum();
    for start in 0..=(r.len() - window) {
        if start > 0 {
            energy += r[start + window - 1].powi(2) - r[start - 1].powi(2);
        }
        if energy > best_energy {
            best_energy = energy;
            best_start = start;
        }
    }
    let max_lag = max_lag as i64;
    let score = |lag: i64| -> f64 {
        (best_start..best_start + window)
            .filter_map(|i| {
                let j = i as i64 + lag;
                (j >= 0 && (j as usize) < t.len()).then(|| r[i] * t[j as usize])
            })
            .sum()
    };
    let lag = (-max_lag..=max_lag)
        .map(|lag| (lag, score(lag)))
        .fold(
            (0i64, f64::MIN),
            |best, item| if item.1 > best.1 { item } else { best },
        )
        .0;
    let (mut signal, mut noise) = (0.0f64, 0.0f64);
    for frame in 0..reference.frames() {
        let other = frame as i64 + lag;
        for channel in 0..channels {
            let a = f64::from(reference.samples[frame * channels + channel]);
            let b = if other >= 0 && (other as usize) < test.frames() {
                f64::from(test.samples[other as usize * channels + channel])
            } else {
                0.0
            };
            signal += a * a;
            noise += (a - b).powi(2);
        }
    }
    if signal <= 0.0 {
        return None;
    }
    let snr = if noise <= 0.0 {
        -LEVEL_FLOOR_DB
    } else {
        (10.0 * (signal / noise).log10()).min(-LEVEL_FLOOR_DB)
    };
    snr.is_finite().then_some(Alignment {
        lag_frames: lag,
        snr_db: snr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, channels: usize, frequency: f64, amplitude: f64, seconds: f64) -> Pcm {
        let frames = (f64::from(rate) * seconds) as usize;
        let mut samples = Vec::with_capacity(frames * channels);
        for i in 0..frames {
            let value = amplitude
                * (2.0 * std::f64::consts::PI * frequency * i as f64 / f64::from(rate)).sin();
            for _ in 0..channels {
                samples.push(value as f32);
            }
        }
        Pcm {
            sample_rate: rate,
            channels,
            samples,
        }
    }

    #[test]
    fn bs1770_reference_tone_has_expected_loudness_at_common_rates() {
        // Синус 997 Гц с пиком −20 dBFS в одном канале даёт −23.0 LUFS (BS.1770).
        for rate in [22050, 24000, 32000, 44100, 48000] {
            let pcm = sine(rate, 1, 997.0, 0.1, 3.0);
            let measured = loudness(&pcm).integrated.unwrap().lufs;
            assert!((measured + 23.0).abs() < 0.15, "{rate}: {measured}");
            let short = loudness(&pcm).short_window.unwrap().lufs;
            assert!((short - measured).abs() < 0.2, "{rate}: {short}");
        }
        // Два одинаковых канала складываются энергетически: +3 dB.
        let stereo = loudness(&sine(48000, 2, 997.0, 0.1, 3.0))
            .integrated
            .unwrap();
        assert!((stereo.lufs + 19.99).abs() < 0.15, "{}", stereo.lufs);
    }

    #[test]
    fn short_clip_has_only_short_window_loudness_and_silence_has_none() {
        let short = sine(44100, 1, 997.0, 0.1, 0.25);
        let measured = loudness(&short);
        assert!(measured.integrated.is_none());
        assert!((measured.short_window.unwrap().lufs + 23.0).abs() < 0.3);
        let silence = Pcm {
            sample_rate: 44100,
            channels: 1,
            samples: vec![0.0; 44100],
        };
        assert_eq!(
            loudness(&silence),
            Loudness {
                integrated: None,
                short_window: None
            }
        );
        assert_eq!(amplitude_db(sample_peak(&silence)), LEVEL_FLOOR_DB);
    }

    #[test]
    fn true_peak_detects_intersample_overshoot() {
        // Синус fs/4 с фазой 45°: отсчёты на 0.707, а реальный пик — 1.0.
        let rate = 48000;
        let samples: Vec<f32> = (0..4800)
            .map(|i| {
                (2.0 * std::f64::consts::PI * 12000.0 * i as f64 / f64::from(rate)
                    + std::f64::consts::FRAC_PI_4)
                    .sin() as f32
            })
            .collect();
        let pcm = Pcm {
            sample_rate: rate,
            channels: 1,
            samples,
        };
        assert!((amplitude_db(sample_peak(&pcm)) + 3.01).abs() < 0.05);
        let tp = amplitude_db(true_peak(&pcm));
        assert!(tp > -0.5 && tp < 0.2, "{tp}");
    }

    #[test]
    fn frame_levels_clipping_and_percentiles_are_finite() {
        let mut pcm = sine(8000, 1, 440.0, 1.2, 0.1);
        let levels = frame_levels(&pcm, 80);
        assert_eq!(levels.len(), 10);
        assert!(levels.iter().all(|l| l.is_finite()));
        let clip = clipping(&pcm);
        assert!(clip.over_full_scale_samples > 0 && clip.clip_runs > 0);
        apply_gain_and_fades(&mut pcm, 0.5, 8, 8);
        assert_eq!(pcm.samples[0], 0.0);
        assert!(percentile(&[], 0.5).is_none());
        assert_eq!(percentile(&[3.0, 1.0, 2.0], 0.5), Some(2.0));
    }

    #[test]
    fn alignment_finds_lag_and_reports_snr() {
        let reference = sine(16000, 1, 300.0, 0.5, 0.5);
        let mut shifted = reference.clone();
        shifted.samples.rotate_right(5);
        let alignment = aligned_snr(&reference, &shifted, 32).unwrap();
        assert_eq!(alignment.lag_frames, 5);
        assert!(alignment.snr_db > 30.0, "{}", alignment.snr_db);
        let identical = aligned_snr(&reference, &reference, 4).unwrap();
        assert_eq!(identical.snr_db, 120.0);
    }

    #[test]
    fn stereo_difference_detects_dual_mono() {
        assert!(stereo_difference_db(&sine(8000, 2, 440.0, 0.5, 0.2)).unwrap() <= -119.0);
        assert!(stereo_difference_db(&sine(8000, 1, 440.0, 0.5, 0.2)).is_none());
    }
}
