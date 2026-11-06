//! Детерминированные акустические измерения декодированного PCM.
//!
//! Модуль не запускает внешних процессов и не знает о файлах: он получает
//! чередующиеся по каналам (interleaved) отсчёты `f32` и возвращает конечные
//! числа либо `None`, если величина для сигнала не определена. Ни одна
//! функция не возвращает NaN/бесконечность. Эвристики описывают уровень
//! сигнала, а не японскую речь.

/// Нижняя граница уровней: цифровая тишина представляется этим числом, а не `-inf`.
pub const LEVEL_FLOOR_DB: f64 = -120.0;

/// PCM с чередованием каналов в кадре (interleaved), исходными частотой и числом каналов.
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
/// Сдвиг ищется по трём окнам, чтобы не перемножать весь файл: окну
/// наибольшей энергии и окнам, захватывающим начало и конец звука.
/// Квазипериодический сигнал (тон, гласная) внутри стационарного окна почти
/// одинаково коррелирует при сдвигах, кратных периоду; переходы между тишиной
/// и звуком на обоих краях штрафуют такой сдвиг в любую сторону и делают его
/// однозначным.
///
/// Корреляция в каждом окне нормирована на энергии обоих сигналов.
/// Взаимная корреляция складывается **по каналам**, а не по их среднему:
/// у стерео с противоположной фазой (L ≈ −R) среднее каналов равно нулю, и
/// выбор сдвига по нему был бы произвольным. При равных оценках выбирается
/// наименьший по модулю сдвиг, поэтому вырожденный сигнал не даёт ложного
/// граничного сдвига. `None` — сигнал эталона в окне не определяет сдвиг
/// (нулевая энергия) либо раскладки различаются.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Alignment {
    pub lag_frames: i64,
    pub snr_db: f64,
}

pub fn aligned_snr(reference: &Pcm, test: &Pcm, max_lag: usize) -> Option<Alignment> {
    if reference.channels != test.channels
        || reference.sample_rate != test.sample_rate
        || reference.channels == 0
        || reference.frames() == 0
        || test.frames() == 0
    {
        return None;
    }
    let channels = reference.channels;
    let (r, t) = (&reference.samples, &test.samples);
    let (reference_frames, test_frames) = (reference.frames(), test.frames());
    let frame_energy = |frame: usize| -> f64 {
        r[frame * channels..(frame + 1) * channels]
            .iter()
            .map(|s| f64::from(*s).powi(2))
            .sum()
    };
    let window = 4096.min(reference_frames);
    let mut best_start = 0;
    let mut best_energy = -1.0;
    let mut energy: f64 = (0..window).map(frame_energy).sum();
    for start in 0..=(reference_frames - window) {
        if start > 0 {
            energy += frame_energy(start + window - 1) - frame_energy(start - 1);
        }
        if energy > best_energy {
            best_energy = energy;
            best_start = start;
        }
    }
    if best_energy <= 0.0 {
        return None;
    }
    // Начало и конец звука: первый и последний кадр не тише −30 dB от самого
    // громкого; окно выходит за переход на четверть своей длины.
    let peak_frame = (0..reference_frames).map(frame_energy).fold(0.0, f64::max);
    let audible = |frame: &usize| frame_energy(*frame) >= peak_frame * 1e-3;
    let onset = (0..reference_frames).find(audible).unwrap_or(0);
    let offset = (0..reference_frames).rev().find(audible).unwrap_or(0);
    let last_start = reference_frames - window;
    let onset_start = onset.saturating_sub(window / 4).min(last_start);
    let offset_start = (offset + window / 4).saturating_sub(window).min(last_start);
    let mut starts = vec![best_start, onset_start, offset_start];
    starts.sort_unstable();
    starts.dedup();
    let max_lag = max_lag as i64;
    // Нормированная взаимная корреляция в каждом окне: сырая сумма r·t росла
    // бы просто от сдвига окна проверяемого сигнала в более громкую часть, и
    // нарастание громкости на краю давало бы ложный сдвиг. Сумма по окнам:
    // истинный сдвиг даёт около 1 в каждом окне.
    let mut test_prefix = Vec::with_capacity(test_frames + 1);
    test_prefix.push(0.0f64);
    let mut running = 0.0f64;
    for frame in 0..test_frames {
        running += t[frame * channels..(frame + 1) * channels]
            .iter()
            .map(|s| f64::from(*s).powi(2))
            .sum::<f64>();
        test_prefix.push(running);
    }
    let window_energy: Vec<f64> = starts
        .iter()
        .map(|&start| (start..start + window).map(frame_energy).sum())
        .collect();
    let score = |lag: i64| -> f64 {
        starts
            .iter()
            .zip(&window_energy)
            .map(|(&start, &reference_energy)| {
                let from = (start as i64 + lag).clamp(0, test_frames as i64) as usize;
                let to = (start as i64 + lag + window as i64).clamp(0, test_frames as i64) as usize;
                let test_energy = test_prefix[to] - test_prefix[from];
                if reference_energy <= 0.0 || test_energy <= 0.0 {
                    return 0.0;
                }
                let cross: f64 = (start..start + window)
                    .filter_map(|i| {
                        let j = i as i64 + lag;
                        (j >= 0 && (j as usize) < test_frames).then(|| {
                            (0..channels)
                                .map(|c| {
                                    f64::from(r[i * channels + c])
                                        * f64::from(t[j as usize * channels + c])
                                })
                                .sum::<f64>()
                        })
                    })
                    .sum();
                cross / (reference_energy * test_energy).sqrt()
            })
            .sum()
    };
    let lag = (-max_lag..=max_lag)
        .map(|lag| (lag, score(lag)))
        .fold((0i64, f64::NEG_INFINITY), |best, item| {
            if item.1 > best.1 || (item.1 == best.1 && item.0.abs() < best.0.abs()) {
                item
            } else {
                best
            }
        })
        .0;
    let (mut signal, mut noise) = (0.0f64, 0.0f64);
    for frame in 0..reference_frames {
        let other = frame as i64 + lag;
        for channel in 0..channels {
            let a = f64::from(r[frame * channels + channel]);
            let b = if other >= 0 && (other as usize) < test_frames {
                f64::from(t[other as usize * channels + channel])
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

    /// Стерео с противоположной фазой каналов и почти противофазное: сдвиг
    /// определяется по каждому каналу, а не по взаимно уничтоженному среднему.
    #[test]
    fn alignment_handles_opposite_phase_stereo_and_true_shift() {
        let mono = sine(16000, 1, 300.0, 0.5, 0.5);
        let opposite = |scale: f32| Pcm {
            sample_rate: 16000,
            channels: 2,
            samples: mono
                .samples
                .iter()
                .flat_map(|s| [*s, -*s * scale])
                .collect(),
        };
        for scale in [1.0, 0.98] {
            let reference = opposite(scale);
            let same = aligned_snr(&reference, &reference, 80).unwrap();
            assert_eq!(same.lag_frames, 0, "{scale}");
            assert_eq!(same.snr_db, 120.0);
            let mut shifted = reference.clone();
            shifted.samples.rotate_right(2 * 7);
            let found = aligned_snr(&reference, &shifted, 80).unwrap();
            assert_eq!(found.lag_frames, 7, "{scale}");
            assert!(found.snr_db > 30.0, "{}", found.snr_db);
        }
        // Реальное повреждение содержимого не скрывается выравниванием.
        let reference = opposite(1.0);
        let mut damaged = reference.clone();
        for value in damaged.samples.iter_mut().skip(4000).take(8000) {
            *value = 0.0;
        }
        let damaged = aligned_snr(&reference, &damaged, 80).unwrap();
        assert_eq!(damaged.lag_frames, 0);
        assert!(damaged.snr_db < 4.0, "{}", damaged.snr_db);
        // Нулевая энергия эталона не определяет сдвиг: результата нет.
        let silent = Pcm {
            sample_rate: 16000,
            channels: 2,
            samples: vec![0.0; 3200],
        };
        assert!(aligned_snr(&silent, &silent, 80).is_none());
    }

    /// Периодический сигнал с краевой тишиной и шумом, похожим на ошибку
    /// кодека: сдвиг на целое число периодов не выдаётся за лучшее
    /// совпадение, настоящий сдвиг находится точно.
    #[test]
    fn periodic_signal_is_not_aligned_to_a_whole_period_shift() {
        let rate = 44100;
        let tone = sine(rate, 1, 440.0, 0.3, 0.8);
        let mut samples = vec![0.0f32; 2646];
        samples.extend(&tone.samples);
        samples.extend(vec![0.0f32; 2646]);
        let reference = Pcm {
            sample_rate: rate,
            channels: 1,
            samples,
        };
        let mut state = 0x9e37_79b9u32;
        let mut noisy = reference.clone();
        for value in &mut noisy.samples {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *value += ((state >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.02;
        }
        let found = aligned_snr(&reference, &noisy, 1323).unwrap();
        assert_eq!(found.lag_frames, 0);
        let mut shifted = noisy.clone();
        shifted.samples.rotate_right(37);
        assert_eq!(
            aligned_snr(&reference, &shifted, 1323).unwrap().lag_frames,
            37
        );
        shifted.samples.rotate_left(37 + 20);
        assert_eq!(
            aligned_snr(&reference, &shifted, 1323).unwrap().lag_frames,
            -20
        );
    }

    /// Нарастание громкости у края: сырая корреляция росла бы от сдвига окна
    /// в более громкую часть и давала ложный сдвиг; нормированная — нет.
    #[test]
    fn crescendo_at_the_edge_does_not_shift_alignment() {
        let rate = 44100;
        let frames = rate as usize;
        let mut state = 0x1234_abcdu32;
        let samples: Vec<f32> = (0..frames)
            .map(|i| {
                let t = i as f64 / f64::from(rate);
                let envelope = (t / 0.4).min(1.0) * (1.0 - (t - 0.7).max(0.0) / 0.3);
                (0.4 * envelope * (2.0 * std::f64::consts::PI * 180.0 * t).sin()) as f32
            })
            .collect();
        let reference = Pcm {
            sample_rate: rate,
            channels: 1,
            samples,
        };
        let mut noisy = reference.clone();
        for value in &mut noisy.samples {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *value += ((state >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.004;
        }
        let found = aligned_snr(&reference, &noisy, 1323).unwrap();
        assert_eq!(found.lag_frames, 0);
        assert!(found.snr_db > 25.0, "{}", found.snr_db);
    }

    /// Коэффициенты K-фильтра при 48 kHz совпадают с таблицами 1 и 2
    /// ITU-R BS.1770-4: у RLB high-pass числитель ровно `[1, −2, 1]`, без
    /// деления на `a0` (так же в FFmpeg `f_ebur128.c` и libebur128).
    #[test]
    fn k_weighting_matches_bs1770_tables_at_48k() {
        let [shelf, high_pass] = k_weighting(48000);
        let expected_shelf = (
            [
                1.535_124_859_586_97,
                -2.691_696_189_406_38,
                1.198_392_810_852_85,
            ],
            [1.0, -1.690_659_293_182_41, 0.732_480_774_215_85],
        );
        let expected_high_pass = (
            [1.0, -2.0, 1.0],
            [1.0, -1.990_047_454_833_98, 0.990_072_250_366_21],
        );
        for (actual, expected) in [(shelf, expected_shelf), (high_pass, expected_high_pass)] {
            for (a, e) in actual
                .b
                .iter()
                .zip(expected.0)
                .chain(actual.a.iter().zip(expected.1))
            {
                assert!((a - e).abs() < 1e-8, "{a} != {e}");
            }
        }
    }

    #[test]
    fn stereo_difference_detects_dual_mono() {
        assert!(stereo_difference_db(&sine(8000, 2, 440.0, 0.5, 0.2)).unwrap() <= -119.0);
        assert!(stereo_difference_db(&sine(8000, 1, 440.0, 0.5, 0.2)).is_none());
    }

    #[test]
    fn stereo_difference_measures_distinct_channels() {
        let scaled = |right_gain: f32| {
            let mut pcm = sine(8000, 2, 440.0, 0.5, 0.2);
            for frame in pcm.samples.as_chunks_mut::<2>().0 {
                frame[1] *= right_gain;
            }
            pcm
        };
        // Правый канал вдвое тише: (1 − 0,5)² / ((1 + 0,25) / 2) = 0,4 → −3,98 dB.
        let half = stereo_difference_db(&scaled(0.5)).unwrap();
        assert!((half - 10.0 * 0.4f64.log10()).abs() < 0.01, "{half}");
        // Разница в 1 % уровня (≈ −40 dB) — уже не двойное моно для порога −60 dB.
        let near = stereo_difference_db(&scaled(0.99)).unwrap();
        assert!((-41.0..-39.0).contains(&near), "{near}");
        assert!(
            near > super::super::normalizer::NormalizerProfile::balanced()
                .output
                .identical_channels_max_db
        );
        // Независимые каналы разной частоты далеки от двойного моно.
        let mut distinct = sine(8000, 2, 440.0, 0.5, 0.2);
        let other = sine(8000, 1, 660.0, 0.5, 0.2);
        for (frame, value) in distinct
            .samples
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .zip(other.samples)
        {
            frame[1] = value;
        }
        assert!(stereo_difference_db(&distinct).unwrap() > -10.0);
    }
}
