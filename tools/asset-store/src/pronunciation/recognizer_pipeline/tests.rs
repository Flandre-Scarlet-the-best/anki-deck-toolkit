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

fn occurrence_at(core: SampleSpan) -> Occurrence {
    Occurrence {
        index: 0,
        phonemes: "n a m a".into(),
        frames: (0, 0),
        core,
        start_uncertainty: SampleSpan {
            start: core.start.saturating_sub(480),
            end: core.start,
        },
        end_uncertainty: SampleSpan {
            start: core.end,
            end: core.end + 480,
        },
        score: 0.0,
        score_per_token: 0.0,
        free_phonemes: String::new(),
        free_distance: 0,
        best_competitor: None,
        competitor_margin: None,
        verifier_text: None,
        verifier_distance: None,
        snr_db: None,
        evidence: recognizer::Evidence::Match,
        reasons: Vec::new(),
    }
}

#[test]
fn derived_record_spans_return_to_source_scale() {
    // Balanced срезал 0,1 с исходника 44,1 kHz и вывел 48 kHz: секунда вывода
    // — это секунда исходника после среза.
    let span = SampleSpan {
        start: 48_000,
        end: 96_000,
    };
    assert_eq!(
        to_source(span, 4_410, 48_000, 44_100),
        SampleSpan {
            start: 48_510,
            end: 92_610,
        }
    );
    // Без среза и пересчёта шкала не меняется.
    assert_eq!(to_source(span, 0, 48_000, 48_000), span);
}

#[test]
fn fallback_matching_keeps_order_and_moves_every_span_to_source() {
    let first = SampleSpan {
        start: 9_600,
        end: 19_200,
    };
    let second = SampleSpan {
        start: 28_800,
        end: 38_400,
    };
    let matching = Matching {
        target_variants: vec!["n a m a".into()],
        competitors: 0,
        occurrences: vec![occurrence_at(first), occurrence_at(second)],
    };
    let moved = matching_on_source(matching.clone(), 1_600, 48_000, 16_000);
    assert_eq!(moved.occurrences.len(), 2);
    for (before, after) in matching.occurrences.iter().zip(&moved.occurrences) {
        let expect = |s: SampleSpan| SampleSpan {
            start: s.start / 3 + 1_600,
            end: s.end / 3 + 1_600,
        };
        assert_eq!(after.core, expect(before.core));
        assert_eq!(after.start_uncertainty, expect(before.start_uncertainty));
        assert_eq!(after.end_uncertainty, expect(before.end_uncertainty));
        assert_eq!(after.phonemes, before.phonemes);
    }
}

#[test]
fn whole_target_without_balanced_candidate_ends_without_panic() {
    // Balanced отвергает запись как слишком тихую и не выбирает байтов, а
    // модель слышит цель: итогового кандидата нет, этап 2 не нужен.
    let mut f = fixture();
    f.base_profile.analysis.signal_floor_dbfs = -20.0;
    f.base_profile.trim.silence_threshold_dbfs = -40.0;
    let (bytes, _) = scene(&[Seg::Sil(400.0), Seg::Word(1, -30.0), Seg::Sil(400.0)]);
    let r = run(&f, &model(), &bytes, Some(&target(1, &[0, 2]))).unwrap();
    let res = &r.result;
    res.validate().unwrap();
    assert!(res.full_record.as_ref().unwrap().proposed_variant.is_none());
    assert_eq!(
        res.outcome,
        RecognitionOutcome::PostValidationFailed,
        "{:?}",
        res.reasons.iter().map(|r| &r.code).collect::<Vec<_>>()
    );
    assert_eq!(res.next_action, RecognitionAction::KeepFullRecord);
    assert!(res.output.is_none() && r.output_bytes.is_none());
    assert!(
        res.reasons
            .iter()
            .any(|r| r.code == "whole_record_candidate_unavailable")
    );
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
    let post = res.post_validation.as_ref().unwrap();
    assert!(post.passed);
    // Обе границы измерены и укладываются в допуск.
    let tolerance = f.profile.post.timing_tolerance_ms;
    let (start_err, end_err) = (post.start_error_ms.unwrap(), post.end_error_ms.unwrap());
    assert!(start_err <= tolerance && end_err <= tolerance, "{post:?}");
    assert_eq!(post.timing_error_ms, Some(start_err.max(end_err)));
    // Интервал выдаваемого кандидата на шкале исходника лежит внутри
    // извлечения и содержит ядро.
    let processing = res.processing.as_ref().unwrap();
    let delivered = processing.output_span.expect("интервал кандидата");
    assert!(plan.extraction.contains(&delivered) && delivered.contains(&plan.core));
    // Полная безопасная запись сохраняется всегда.
    assert!(res.full_record.is_some());
}

/// Этап 1 извлечения цели, затем подмена итогового кандидата файлом, PCM
/// которого изменён `edit` (кадры кандидата, ожидаемое ядро в них), и этап 2.
fn post_validate_edited(edit: impl Fn(&mut Vec<f32>, SampleSpan, u32)) -> RecognitionResult {
    let f = fixture();
    let (bytes, _) = scene(&[
        Seg::Sil(400.0),
        Seg::Word(0, -14.0),
        Seg::Sil(450.0),
        Seg::Word(1, -14.0),
        Seg::Sil(450.0),
        Seg::Word(2, -14.0),
        Seg::Sil(400.0),
    ]);
    let tone = model();
    let ctx = Context {
        profile: &f.profile,
        primary: &f.primary,
        verifier: Some(&f.verifier),
        base_profile: &f.base_profile,
        base_toolchain: &f.toolchain,
        enhancer: None,
        config_sha256: "synthetic-config",
        source: &tone,
    };
    let mut stage = stage_one(&bytes, Some(&target(1, &[0, 2])), &ctx).unwrap();
    assert_eq!(stage.pending, Some(RecognitionOutcome::TargetExtracted));
    let expected = stage
        .result
        .processing
        .as_ref()
        .unwrap()
        .expected_core
        .unwrap();
    let candidate = stage.output_bytes.as_deref().unwrap();
    let decoded = decode(candidate, &f.base_profile).unwrap().unwrap();
    assert_eq!(decoded.pcm.channels, 1);
    let rate = decoded.timeline.sample_rate;
    let mut samples = decoded.pcm.samples;
    edit(&mut samples, expected, rate);
    stage.output_bytes =
        Some(probe::encode_mp3(&samples, rate, 1, rate, &["-q:a".into(), "2".into()]).unwrap());
    stage_two(stage, &ctx).unwrap().result
}

fn assert_end_rejected(result: &RecognitionResult) {
    assert_eq!(result.outcome, RecognitionOutcome::PostValidationFailed);
    assert_ne!(result.next_action, RecognitionAction::UseFinalCandidate);
    let post = result.post_validation.as_ref().unwrap();
    let tolerance = RecognitionProfile::conservative().post.timing_tolerance_ms;
    // Начало не тронуто: отказ — именно по концу слова.
    assert!(post.start_error_ms.unwrap() <= tolerance, "{post:?}");
    assert!(post.end_error_ms.unwrap() > tolerance, "{post:?}");
    let timing = post
        .checks
        .iter()
        .find(|c| c.id == "timing_consistent")
        .unwrap();
    assert!(!timing.passed, "{:?}", post.checks);
}

#[test]
fn post_validation_rejects_prematurely_cut_word_end() {
    let result = post_validate_edited(|samples, expected, rate| {
        // Конец слова срезан на 150 мс, начало и всё до него без изменений.
        let cut = expected.end - u64::from(rate) * 150 / 1000;
        samples.truncate(cut as usize);
    });
    assert_end_rejected(&result);
}

#[test]
fn post_validation_rejects_noticeably_extended_word_end() {
    let result = post_validate_edited(|samples, expected, rate| {
        // Тот же тон продолжается ещё 200 мс после конца слова.
        let from = expected.end as usize;
        let len = (rate as usize) * 200 / 1000;
        let tail = (rate as usize) * 150 / 1000;
        samples.resize(samples.len().max(from + len + tail), 0.0);
        let amp = 10f64.powf(-14.0 / 20.0);
        for (i, v) in samples[from..from + len].iter_mut().enumerate() {
            let p = 2.0 * std::f64::consts::PI * WORDS[1].0 * i as f64 / f64::from(rate);
            *v = (amp * (p.sin() + 0.3 * (2.0 * p).sin())) as f32;
        }
        samples[from + len..].fill(0.0);
    });
    assert_end_rejected(&result);
}

#[test]
fn post_validation_accepts_unchanged_reencoded_candidate() {
    let result = post_validate_edited(|_, _, _| {});
    let post = result.post_validation.as_ref().unwrap();
    assert!(post.passed, "{:?}", post.checks);
    assert_eq!(result.outcome, RecognitionOutcome::TargetExtracted);
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

#[test]
fn evaluation_measures_the_delivered_file_not_the_primary_extraction() {
    use crate::pronunciation::recognizer_eval::{LabelWord, boundary_metrics};
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
    assert_eq!(r.result.outcome, RecognitionOutcome::TargetExtracted);
    let words: Vec<LabelWord> = truth
        .iter()
        .map(|(w, span)| LabelWord {
            reading: WORDS[*w].2.into(),
            start_ms: to_ms(span.start),
            end_ms: to_ms(span.end),
        })
        .collect();
    let rate = f64::from(RATE);
    let ok = boundary_metrics(&r.result, &words, Some(1), rate);
    assert_eq!(ok.delivered_boundaries_resolved, Some(true));
    assert_eq!(ok.extraction_cuts_target, Some(false));
    assert_eq!(ok.extraction_contains_other_words, Some(false));

    // Последующая обрезка срезает 100 мс конца слова, а первичное
    // извлечение его покрывало. Длина выданного файла согласована с
    // обрезкой, поэтому границы установлены — и обрезка видна.
    let (_, word) = truth[1];
    let cut = u64::from(RATE) / 10;
    let mut trimmed = r.result.clone();
    let processing = trimmed.processing.as_mut().unwrap();
    let span = processing.output_span.as_mut().unwrap();
    let removed = span.end - (word.end - cut);
    span.end = word.end - cut;
    let probe = trimmed
        .post_validation
        .as_mut()
        .unwrap()
        .output_probe
        .as_mut()
        .unwrap();
    probe.decoded_samples -= removed * u64::from(probe.sample_rate) / u64::from(RATE);
    let m = boundary_metrics(&trimmed, &words, Some(1), rate);
    assert_eq!(m.primary_extraction_cuts_target, Some(false));
    assert_eq!(m.extraction_cuts_target, Some(true));

    // Длина выданного файла не согласуется с интервалом: связь шкал не
    // доказана, метрика неразрешима, а не «обрезки нет».
    let mut unknown = r.result.clone();
    let probe = unknown
        .post_validation
        .as_mut()
        .unwrap()
        .output_probe
        .as_mut()
        .unwrap();
    probe.decoded_samples += u64::from(probe.sample_rate) / 20;
    let m = boundary_metrics(&unknown, &words, Some(1), rate);
    assert_eq!(m.delivered_boundaries_resolved, Some(false));
    assert_eq!(m.extraction_cuts_target, None);
    assert_eq!(m.primary_extraction_cuts_target, Some(false));
}

#[test]
fn worker_panic_is_an_explicit_item_error_not_a_missing_item() {
    let inputs: Vec<usize> = (0..10).collect();
    let stop = std::sync::atomic::AtomicBool::new(false);
    let (results, panicked) = run_parallel("этап 1", &inputs, 3, &stop, |&i| {
        if i == 4 {
            panic!("смоделированный сбой элемента {i}");
        }
        Ok(i * 2)
    });
    assert!(panicked.is_none());
    // Каждый вход учтён ровно один раз: паника не превращается в пропажу.
    assert_eq!(results.iter().map(|(i, _)| *i).collect::<Vec<_>>(), inputs);
    for (i, result) in &results {
        match result {
            Ok(v) => assert_eq!(*v, i * 2),
            Err(error) => {
                assert_eq!(*i, 4);
                assert_eq!(error.details["blocker"], "worker_panicked");
                assert!(error.message.contains("смоделированный сбой элемента 4"));
                // Не сбой среды: пакет не повторяет его молча как временный.
                assert!(!is_infrastructure_failure(error));
            }
        }
    }
    assert_eq!(results.iter().filter(|(_, r)| r.is_err()).count(), 1);
}
