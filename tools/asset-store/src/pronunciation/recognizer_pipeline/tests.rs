//! Весь конвейер на настоящих MP3 и поддельной модели с известным поведением:
//! «слово» — тональная вспышка с собственной основной частотой, и поддельный
//! источник эмиссий узнаёт слово по частоте, а не по положению. Поэтому
//! повторное распознавание вырезанного фрагмента видит то же слово, что и
//! исходник, а истинные интервалы известны из построения сцены.

use super::*;
use crate::pronunciation::recognizer::{RecognitionAction, TargetSource, TargetSourceKind};
use crate::pronunciation::recognizer_backend::classes_for;

const PHONEMES: [&str; 48] = [
    "PAD", "UNK", "SOS", "EOS", "a", "i", "u", "e", "o", "I", "U", "k", "g", "s", "z", "t", "d",
    "n", "h", "b", "p", "m", "y", "r", "w", "f", "j", "v", "N", "cl", "sh", "ch", "ts", "ky", "gy",
    "hy", "by", "py", "my", "ny", "ry", "fy", "ty", "dy", "kw", "gw", "pau", "sil",
];
const HIRAGANA: [&str; 16] = [
    "[PAD]", "な", "ま", "せ", "い", "し", "ょ", "う", "き", "っ", "て", "ほ", "ん", "か", "あ",
    "ぱ",
];
const RATE: u32 = 44_100;
const STRIDE: usize = 320;

/// Словарь сцены: основная частота, фонемы основной модели, хирагана.
const WORDS: [(f64, &str, &str); 4] = [
    (190.0, "s e i", "せい"),
    (310.0, "n a m a", "なま"),
    (500.0, "sh o o", "しょう"),
    (760.0, "k i t e", "きて"),
];

fn identity(vocab: &[&str], output: &str) -> ModelIdentity {
    ModelIdentity {
        model_id: format!("synthetic-{output}"),
        output: output.into(),
        repository: String::new(),
        revision: String::new(),
        license: String::new(),
        manifest_sha256: String::new(),
        worker_sha256: String::new(),
        worker_byte_length: 0,
        worker_version: String::new(),
        inference: String::new(),
        device: "cpu".into(),
        threads: 1,
        sample_rate: 16_000,
        frame_stride: STRIDE,
        receptive_field: 400,
        lead_pad_samples: 0,
        trail_pad_samples: 0,
        blank_id: 0,
        vocab: vocab.iter().map(|s| s.to_string()).collect(),
    }
}

fn handle(vocab: &[&str], output: &str) -> ModelHandle {
    let identity = identity(vocab, output);
    ModelHandle {
        classes: classes_for(&identity),
        identity,
        model_dir: std::path::PathBuf::from("synthetic"),
        worker: std::path::PathBuf::from("synthetic"),
        address_space: 0,
    }
}

/// Поддельная модель: вспышки энергии в кадрах 20 мс, слово вспышки — по
/// ближайшей основной частоте (Гёрцель), токены слова равномерно по вспышке.
struct ToneModel {
    fail: Option<&'static str>,
    calls: std::sync::Mutex<usize>,
}

fn goertzel(x: &[f32], f: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * f / 16_000.0;
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &v in x {
        let s = v as f64 + 2.0 * w.cos() * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2
}

impl EmissionSource for ToneModel {
    fn emissions(
        &self,
        model: &ModelHandle,
        inputs: &[&[f32]],
    ) -> Result<Vec<Emissions>, AssetError> {
        *self.calls.lock().unwrap() += 1;
        if let Some(code) = self.fail {
            return Err(ebackend::backend_failure(
                code,
                "поддельный сбой воркера",
                serde_json::json!({}),
            ));
        }
        let vocab = &model.identity.vocab;
        let phonemes = model.identity.output == "phoneme";
        Ok(inputs
            .iter()
            .map(|x| tone_emissions(x, vocab, phonemes))
            .collect())
    }
}

fn tone_emissions(x: &[f32], vocab: &[String], phonemes: bool) -> Emissions {
    let frames = x.len() / STRIDE;
    let db: Vec<f64> = (0..frames)
        .map(|f| {
            let w = &x[f * STRIDE..(f + 1) * STRIDE];
            let e = w.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / STRIDE as f64;
            10.0 * (e + 1e-12).log10()
        })
        .collect();
    let mut tokens = vec![0usize; frames];
    let mut f = 0;
    while f < frames {
        if db[f] < -45.0 {
            f += 1;
            continue;
        }
        let start = f;
        while f < frames && db[f] >= -45.0 {
            f += 1;
        }
        let end = f;
        let burst = &x[start * STRIDE..end * STRIDE];
        let (_, units, hira) = WORDS
            .iter()
            .max_by(|a, b| goertzel(burst, a.0).total_cmp(&goertzel(burst, b.0)))
            .unwrap();
        let symbols: Vec<String> = if phonemes {
            units.split(' ').map(str::to_string).collect()
        } else {
            hira.chars().map(|c| c.to_string()).collect()
        };
        let len = end - start;
        for (i, symbol) in symbols.iter().enumerate() {
            let id = vocab
                .iter()
                .position(|v| v == symbol)
                .expect("символ в словаре");
            let a = start + i * len / symbols.len();
            let b = start + (i + 1) * len / symbols.len();
            // Блок одного токена с пробелом в конце, чтобы повторы не сливались.
            for t in tokens
                .iter_mut()
                .take(b.saturating_sub(1).max(a + 1))
                .skip(a)
            {
                *t = id;
            }
        }
    }
    let n = vocab.len();
    let rest = (0.1f32 / (n - 1) as f32).ln();
    let values = tokens
        .iter()
        .flat_map(|&t| (0..n).map(move |k| if k == t { 0.9f32.ln() } else { rest }))
        .collect();
    Emissions::new(frames, n, values).unwrap()
}

enum Seg {
    Sil(f64),
    Word(usize, f64),
}

/// Сцена в 44.1 kHz моно → MP3 и истинные интервалы слов в отсчётах.
fn scene(segments: &[Seg]) -> (Vec<u8>, Vec<(usize, SampleSpan)>) {
    let mut samples: Vec<f32> = Vec::new();
    let mut truth = Vec::new();
    for seg in segments {
        match *seg {
            Seg::Sil(ms) => samples.extend(std::iter::repeat_n(
                0.0,
                (ms / 1000.0 * RATE as f64) as usize,
            )),
            Seg::Word(w, db) => {
                let n = (0.42 * RATE as f64) as usize;
                let amp = 10f64.powf(db / 20.0);
                let f0 = WORDS[w].0;
                let start = samples.len() as u64;
                samples.extend((0..n).map(|i| {
                    let t = i as f64 / RATE as f64;
                    let env = (std::f64::consts::PI * i as f64 / n as f64).sin().powf(0.3);
                    let p = 2.0 * std::f64::consts::PI * f0 * t;
                    (amp * env * (p.sin() + 0.3 * (2.0 * p).sin())) as f32
                }));
                truth.push((
                    w,
                    SampleSpan {
                        start,
                        end: samples.len() as u64,
                    },
                ));
            }
        }
    }
    let bytes = probe::encode_mp3(&samples, RATE, 1, RATE, &["-q:a".into(), "2".into()])
        .expect("для теста нужен FFmpeg с libmp3lame");
    (bytes, truth)
}

fn target(w: usize, competing: &[usize]) -> TargetIdentity {
    TargetIdentity {
        surface: "生".into(),
        reading: WORDS[w].2.into(),
        source: TargetSource {
            kind: TargetSourceKind::Request,
            key: "test".into(),
        },
        competing_readings: competing.iter().map(|&c| WORDS[c].2.to_string()).collect(),
    }
}

struct Fixture {
    profile: RecognitionProfile,
    primary: ModelHandle,
    verifier: ModelHandle,
    base_profile: NormalizerProfile,
    toolchain: Toolchain,
}

fn fixture() -> Fixture {
    Fixture {
        profile: RecognitionProfile::conservative(),
        primary: handle(&PHONEMES, "phoneme"),
        verifier: handle(&HIRAGANA, "hiragana"),
        base_profile: NormalizerProfile::balanced(),
        toolchain: Toolchain::detect().expect("для теста нужен FFmpeg"),
    }
}

fn run(
    f: &Fixture,
    model: &ToneModel,
    bytes: &[u8],
    target: Option<&TargetIdentity>,
) -> Result<Recognized, AssetError> {
    let ctx = Context {
        profile: &f.profile,
        primary: &f.primary,
        verifier: Some(&f.verifier),
        base_profile: &f.base_profile,
        base_toolchain: &f.toolchain,
        enhancer: None,
        config_sha256: "synthetic-config",
        source: model,
    };
    recognize_one(bytes, target, &ctx)
}

fn model() -> ToneModel {
    ToneModel {
        fail: None,
        calls: std::sync::Mutex::new(0),
    }
}

fn to_ms(samples: u64) -> f64 {
    samples as f64 * 1000.0 / RATE as f64
}

#[test]
fn shift_saturates_at_zero() {
    let span = SampleSpan { start: 10, end: 50 };
    assert_eq!(shift(span, 20), SampleSpan { start: 0, end: 30 });
}

#[test]
fn single_target_is_accepted_and_post_validated_on_decoded_output() {
    let f = fixture();
    let (bytes, truth) = scene(&[Seg::Sil(400.0), Seg::Word(1, -14.0), Seg::Sil(400.0)]);
    let r = run(&f, &model(), &bytes, Some(&target(1, &[0, 2]))).unwrap();
    let res = &r.result;
    res.validate().unwrap();
    assert!(
        matches!(
            res.outcome,
            RecognitionOutcome::TargetWhole | RecognitionOutcome::TargetExtracted
        ),
        "{:?} {:?}",
        res.outcome,
        res.reasons.iter().map(|r| &r.code).collect::<Vec<_>>()
    );
    assert_eq!(res.next_action, RecognitionAction::UseFinalCandidate);
    let post = res
        .post_validation
        .as_ref()
        .expect("повторная проверка выполнена");
    assert!(post.passed, "{:?}", post.checks);
    assert!(r.output_bytes.is_some());
    let occ = &res.matching.as_ref().unwrap().occurrences[0];
    let (_, span) = truth[0];
    assert!(
        (to_ms(occ.core.start) - to_ms(span.start)).abs() < 60.0,
        "{:?} {:?}",
        occ.core,
        span
    );
    assert!(
        (to_ms(occ.core.end) - to_ms(span.end)).abs() < 60.0,
        "{:?} {:?}",
        occ.core,
        span
    );
    // Свободная гипотеза хранится отдельно и не подстроена под цель.
    let free = &res.pre_analysis.as_ref().unwrap().primary.raw_text;
    assert!(free.contains('n') && free.contains('m'), "{free}");
}

#[test]
fn target_among_other_readings_is_extracted_without_neighbours() {
    let f = fixture();
    let (bytes, truth) = scene(&[
        Seg::Sil(400.0),
        Seg::Word(0, -14.0),
        Seg::Sil(450.0),
        Seg::Word(1, -14.0),
        Seg::Sil(450.0),
        Seg::Word(2, -14.0),
        Seg::Sil(400.0),
    ]);
    let r = run(&f, &model(), &bytes, Some(&target(1, &[0, 2]))).unwrap();
    let res = &r.result;
    res.validate().unwrap();
    assert_eq!(
        res.outcome,
        RecognitionOutcome::TargetExtracted,
        "{:?}",
        res.reasons
    );
    let plan = res.extraction.as_ref().expect("план извлечения");
    let (_, before) = truth[0];
    let (_, own) = truth[1];
    let (_, after) = truth[2];
    assert!(
        plan.extraction.start >= before.end && plan.extraction.end <= after.start,
        "{plan:?}"
    );
    assert!(
        plan.extraction.start <= own.start && plan.extraction.end >= own.end,
        "{plan:?}"
    );
    assert!(res.post_validation.as_ref().unwrap().passed);
    // Полная безопасная запись сохраняется всегда.
    assert!(res.full_record.is_some());
}

#[test]
fn equal_repeats_are_reported_as_multiple_occurrences() {
    let f = fixture();
    let (bytes, _) = scene(&[
        Seg::Sil(400.0),
        Seg::Word(1, -14.0),
        Seg::Sil(450.0),
        Seg::Word(1, -14.0),
        Seg::Sil(400.0),
    ]);
    let r = run(&f, &model(), &bytes, Some(&target(1, &[]))).unwrap();
    assert_eq!(
        r.result.outcome,
        RecognitionOutcome::MultipleOccurrences,
        "{:?}",
        r.result.reasons
    );
    assert_eq!(r.result.next_action, RecognitionAction::PassCandidates);
    assert_eq!(r.result.matching.as_ref().unwrap().occurrences.len(), 2);
}

#[test]
fn absent_target_and_silence_are_not_accepted() {
    let f = fixture();
    let (bytes, _) = scene(&[Seg::Sil(400.0), Seg::Word(3, -14.0), Seg::Sil(400.0)]);
    let r = run(&f, &model(), &bytes, Some(&target(1, &[]))).unwrap();
    assert_eq!(
        r.result.outcome,
        RecognitionOutcome::TargetAbsent,
        "{:?}",
        r.result.reasons
    );
    assert_ne!(r.result.next_action, RecognitionAction::UseFinalCandidate);
    assert!(r.output_bytes.is_none());

    let (silent, _) = scene(&[Seg::Sil(1500.0)]);
    let r = run(&f, &model(), &silent, Some(&target(1, &[]))).unwrap();
    assert_eq!(
        r.result.outcome,
        RecognitionOutcome::NoSpeech,
        "{:?}",
        r.result.reasons
    );
    assert!(r.output_bytes.is_none());
}

#[test]
fn without_target_only_free_analysis_is_recorded() {
    let f = fixture();
    let (bytes, _) = scene(&[Seg::Sil(400.0), Seg::Word(1, -14.0), Seg::Sil(400.0)]);
    let r = run(&f, &model(), &bytes, None).unwrap();
    assert_eq!(
        r.result.outcome,
        RecognitionOutcome::TargetIdentityUnavailable
    );
    assert!(r.result.matching.is_none());
    assert!(r.result.pre_analysis.is_some());
    assert!(r.output_bytes.is_none());
}

#[test]
fn backend_failure_is_infrastructure_error_not_result() {
    let f = fixture();
    let (bytes, _) = scene(&[Seg::Sil(400.0), Seg::Word(1, -14.0), Seg::Sil(400.0)]);
    let failing = ToneModel {
        fail: Some("recognizer_backend_timeout"),
        calls: std::sync::Mutex::new(0),
    };
    let Err(err) = run(&f, &failing, &bytes, Some(&target(1, &[]))) else {
        panic!("сбой воркера не должен стать результатом");
    };
    assert!(is_infrastructure_failure(&err), "{err:?}");
}

#[test]
fn result_is_deterministic() {
    let f = fixture();
    let (bytes, _) = scene(&[
        Seg::Sil(400.0),
        Seg::Word(0, -14.0),
        Seg::Sil(300.0),
        Seg::Word(1, -14.0),
        Seg::Sil(400.0),
    ]);
    let t = target(1, &[0]);
    let a = run(&f, &model(), &bytes, Some(&t)).unwrap();
    let b = run(&f, &model(), &bytes, Some(&t)).unwrap();
    assert_eq!(
        serde_json::to_value(&a.result).unwrap(),
        serde_json::to_value(&b.result).unwrap()
    );
    assert_eq!(a.output_bytes, b.output_bytes);
}
