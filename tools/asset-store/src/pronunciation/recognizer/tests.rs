//! Решения распознавания на синтетических эмиссиях с известной разметкой:
//! истинные кадры каждого слова заданы сценой, а не получены моделью.

use super::*;

const PHONEMES: [&str; 48] = [
    "PAD", "UNK", "SOS", "EOS", "a", "i", "u", "e", "o", "I", "U", "k", "g", "s", "z", "t", "d",
    "n", "h", "b", "p", "m", "y", "r", "w", "f", "j", "v", "N", "cl", "sh", "ch", "ts", "ky", "gy",
    "hy", "by", "py", "my", "ny", "ry", "fy", "ty", "dy", "kw", "gw", "pau", "sil",
];
const HIRAGANA: [&str; 25] = [
    "[PAD]", "|", "な", "ま", "せ", "い", "し", "ょ", "う", "き", "っ", "て", "や", "え", "ご",
    "ほ", "ん", "ど", "じ", "お", "ち", "か", "ば", "あ", "さ",
];
const STRIDE: usize = 320;
const WINDOW: u64 = 160; // 10 мс при 16 kHz

enum Seg {
    /// Слово: фонемы основной модели, хирагана второй модели, уровень dBFS.
    Word(&'static str, &'static str, f64),
    Silence(usize),
    /// Активность без речи (шум, щелчок): модель слышит пробел.
    Noise(usize, f64),
}

struct Scene {
    primary: Emissions,
    verifier: Emissions,
    levels: Vec<f64>,
    frames: usize,
    /// Истинные кадры слов (первый и последний кадр фонем).
    truth: Vec<(usize, usize)>,
}

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

fn row(vocab: usize, token: usize, p: f32) -> Vec<f32> {
    let rest = ((1.0 - p) / (vocab - 1) as f32).ln();
    (0..vocab)
        .map(|k| if k == token { p.ln() } else { rest })
        .collect()
}

fn build(segments: &[Seg]) -> Scene {
    let p_id = |p: &str| PHONEMES.iter().position(|x| *x == p).expect(p);
    let h_id = |c: &str| HIRAGANA.iter().position(|x| *x == c).expect(c);
    let mut prim: Vec<usize> = Vec::new();
    let mut ver: Vec<usize> = Vec::new();
    let mut level: Vec<f64> = Vec::new();
    let mut truth = Vec::new();
    for seg in segments {
        match seg {
            Seg::Silence(n) => {
                prim.extend(std::iter::repeat_n(0, *n));
                ver.extend(std::iter::repeat_n(0, *n));
                level.extend(std::iter::repeat_n(-80.0, *n));
            }
            Seg::Noise(n, db) => {
                prim.extend(std::iter::repeat_n(0, *n));
                ver.extend(std::iter::repeat_n(0, *n));
                level.extend(std::iter::repeat_n(*db, *n));
            }
            Seg::Word(phonemes, hira, db) => {
                let start = prim.len();
                let mut word = Vec::new();
                for p in phonemes.split(' ') {
                    word.extend([p_id(p), p_id(p), 0]);
                }
                word.pop();
                let end = start + word.len() - 1;
                truth.push((start, end));
                // Хирагана второй модели — равномерно по тем же кадрам.
                let chars: Vec<String> = hira.chars().map(|c| c.to_string()).collect();
                let mut v = vec![0; word.len()];
                for (i, c) in chars.iter().enumerate() {
                    let at = i * word.len() / chars.len().max(1);
                    v[at] = h_id(c);
                }
                prim.extend(word);
                ver.extend(v);
                level.extend(std::iter::repeat_n(*db, end + 1 - start));
            }
        }
    }
    let frames = prim.len();
    let em = |tokens: &[usize], vocab: usize| {
        let values = tokens.iter().flat_map(|&t| row(vocab, t, 0.9)).collect();
        Emissions::new(tokens.len(), vocab, values).unwrap()
    };
    // Уровни на окна 10 мс: кадр 20 мс = два окна.
    let levels = level.iter().flat_map(|l| [*l, *l]).collect();
    Scene {
        primary: em(&prim, PHONEMES.len()),
        verifier: em(&ver, HIRAGANA.len()),
        levels,
        frames,
        truth,
    }
}

struct Run {
    pre: PreAnalysis,
    matching: Matching,
    selection: Selection,
}

fn run_with(scene: &Scene, target: TargetIdentity, profile: &RecognitionProfile) -> Run {
    let p_identity = identity(&PHONEMES, "phoneme");
    let v_identity = identity(&HIRAGANA, "hiragana");
    let p_classes = super::super::recognizer_backend::classes_for(&p_identity);
    let v_classes = super::super::recognizer_backend::classes_for(&v_identity);
    let primary = ModelView {
        identity: &p_identity,
        classes: &p_classes,
        emissions: &scene.primary,
    };
    let verifier = ModelView {
        identity: &v_identity,
        classes: &v_classes,
        emissions: &scene.verifier,
    };
    let total = (scene.frames * STRIDE) as u64;
    let timeline = Timeline {
        original_sha256: "0".repeat(64),
        sample_rate: 16_000,
        channels: 1,
        frames: total,
        analysis_sample_rate: 16_000,
        analysis_frames: total,
        analysis_downmix: "mean".into(),
        analysis_resampler: "none".into(),
    };
    let noise = -80.0;
    let activity = activity_regions(
        &scene.levels,
        WINDOW,
        total,
        noise,
        &profile.analysis,
        16_000,
    );
    let pre = pre_analyze(
        timeline,
        primary,
        Some(verifier),
        activity,
        noise,
        &profile.analysis,
    );
    let matching = match_target(
        &pre,
        primary,
        Some(verifier),
        &target,
        &scene.levels,
        WINDOW,
        profile,
    )
    .unwrap();
    let selection = select(&pre, &matching, &scene.levels, WINDOW, profile);
    Run {
        pre,
        matching,
        selection,
    }
}

fn analyze(scene: &Scene, surface: &str, reading: &str) -> Run {
    run_with(
        scene,
        target(surface, reading, &[]),
        &RecognitionProfile::conservative(),
    )
}

fn target(surface: &str, reading: &str, alternatives: &[&str]) -> TargetIdentity {
    TargetIdentity {
        surface: surface.into(),
        reading: reading.into(),
        source: TargetSource {
            kind: TargetSourceKind::Manifest,
            key: "test".into(),
        },
        competing_readings: alternatives.iter().map(|s| s.to_string()).collect(),
    }
}

fn matches(run: &Run) -> Vec<&Occurrence> {
    run.matching
        .occurrences
        .iter()
        .filter(|o| o.evidence == Evidence::Match)
        .collect()
}

fn span_of(frames: (usize, usize)) -> SampleSpan {
    SampleSpan {
        start: (frames.0 * STRIDE + 40) as u64,
        end: ((frames.1 + 1) * STRIDE + 40) as u64,
    }
}

#[test]
fn target_is_extracted_from_several_readings() {
    // 生: せい → なま → しょう, цель なま.
    let scene = build(&[
        Seg::Silence(20),
        Seg::Word("s e e", "せい", -20.0),
        Seg::Silence(25),
        Seg::Word("n a m a", "なま", -20.0),
        Seg::Silence(25),
        Seg::Word("sh o o", "しょう", -20.0),
        Seg::Silence(20),
    ]);
    let run = analyze(&scene, "生", "なま");
    assert_eq!(run.pre.events.len(), 3);
    let found = matches(&run);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].frames, scene.truth[1], "кадры из разметки сцены");
    assert_eq!(run.selection.outcome, RecognitionOutcome::TargetExtracted);
    let plan = run.selection.plan.as_ref().unwrap();
    assert!(plan.extraction.contains(&span_of(scene.truth[1])));
    assert!(!plan.extraction.overlaps(&span_of(scene.truth[0])));
    assert!(!plan.extraction.overlaps(&span_of(scene.truth[2])));
    assert!(plan.context.contains(&plan.extraction));
}

#[test]
fn repeated_target_finds_both_and_selects_only_with_justification() {
    let equal = build(&[
        Seg::Silence(20),
        Seg::Word("n a m a", "なま", -20.0),
        Seg::Silence(25),
        Seg::Word("n a m a", "なま", -20.0),
        Seg::Silence(25),
        Seg::Word("s e e", "せい", -20.0),
        Seg::Silence(20),
    ]);
    let run_equal = analyze(&equal, "生", "なま");
    let found: Vec<(usize, usize)> = matches(&run_equal).iter().map(|o| o.frames).collect();
    assert_eq!(found, [equal.truth[0], equal.truth[1]]);
    assert_eq!(
        run_equal.selection.outcome,
        RecognitionOutcome::MultipleOccurrences
    );
    assert_eq!(
        action_for(run_equal.selection.outcome, true),
        RecognitionAction::PassCandidates
    );
    // Второе произношение заметно чище (выше уровень над шумом) — выбор обоснован.
    let louder = build(&[
        Seg::Silence(20),
        Seg::Word("n a m a", "なま", -40.0),
        Seg::Silence(25),
        Seg::Word("n a m a", "なま", -20.0),
        Seg::Silence(25),
        Seg::Word("s e e", "せい", -20.0),
        Seg::Silence(20),
    ]);
    let run_louder = analyze(&louder, "生", "なま");
    assert_eq!(
        run_louder.selection.outcome,
        RecognitionOutcome::TargetExtracted
    );
    let chosen = run_louder.selection.selected.unwrap();
    assert_eq!(
        run_louder.matching.occurrences[chosen].frames,
        louder.truth[1]
    );
}

#[test]
fn absent_target_and_one_mora_differences_are_not_matches() {
    let scene = build(&[
        Seg::Silence(20),
        Seg::Word("s e e", "せい", -20.0),
        Seg::Silence(20),
    ]);
    let run_absent = analyze(&scene, "生", "なま");
    assert!(matches(&run_absent).is_empty());
    assert!(matches!(
        run_absent.selection.outcome,
        RecognitionOutcome::TargetAbsent | RecognitionOutcome::Ambiguous
    ));
    for (audio, hira, reading) in [
        ("k i t e", "きて", "きって"),
        ("k i cl t e", "きって", "きて"),
        ("h o", "ほ", "ほん"),
        ("o b a s a N", "おばさん", "おばあさん"),
    ] {
        let scene = build(&[
            Seg::Silence(20),
            Seg::Word(audio, hira, -20.0),
            Seg::Silence(20),
        ]);
        let run = analyze(&scene, "語", reading);
        assert!(matches(&run).is_empty(), "{audio} ≠ {reading}");
        assert!(
            !matches!(
                run.selection.outcome,
                RecognitionOutcome::TargetWhole | RecognitionOutcome::TargetExtracted
            ),
            "{audio} ≠ {reading}: {:?}",
            run.selection.outcome
        );
    }
}

#[test]
fn verifier_disagreement_and_explicit_alternatives_block_a_match() {
    // Основная модель слышит «なま», вторая — «かや»: расхождение на две
    // фонемы ошибкой второй модели не объясняется.
    let scene = build(&[
        Seg::Silence(20),
        Seg::Word("n a m a", "かや", -20.0),
        Seg::Silence(20),
    ]);
    let run = analyze(&scene, "生", "なま");
    assert!(matches(&run).is_empty());
    assert!(
        run.matching.occurrences[0]
            .reasons
            .contains(&"verifier_disagrees".to_string())
    );
    assert_eq!(run.selection.outcome, RecognitionOutcome::Ambiguous);
    // Без второй модели строгого совпадения не бывает, если она обязательна.
    let mut profile = RecognitionProfile::conservative();
    profile.matching.verifier_max_distance = 0;
    let scene = build(&[
        Seg::Silence(20),
        Seg::Word("s e e", "せい", -20.0),
        Seg::Silence(20),
    ]);
    let run = run_with(&scene, target("生", "せい", &["しょう", "なま"]), &profile);
    assert_eq!(matches(&run).len(), 1);
    let run = run_with(&scene, target("生", "しょう", &["せい"]), &profile);
    assert!(matches(&run).is_empty());
}

#[test]
fn single_verifier_slip_confirms_only_an_exact_confident_primary() {
    // Вторая модель ошиблась на одну кану («なや»), основная точно слышит
    // «なま» с большим отрывом от всех конкурентов: цель подтверждена, а
    // причина записана.
    let scene = build(&[
        Seg::Silence(20),
        Seg::Word("n a m a", "なや", -20.0),
        Seg::Silence(20),
    ]);
    let run = analyze(&scene, "生", "なま");
    assert_eq!(matches(&run).len(), 1);
    assert!(
        matches(&run)[0]
            .reasons
            .contains(&"verifier_near_match".to_string())
    );
    // С нулевым допуском то же расхождение блокирует совпадение.
    let mut strict = RecognitionProfile::conservative();
    strict.matching.verifier_relaxed_distance = 0;
    let run = run_with(&scene, target("生", "なま", &[]), &strict);
    assert!(matches(&run).is_empty());
    // Если отрыв от конкурентов меньше требуемого, допуск не действует.
    let mut demanding = RecognitionProfile::conservative();
    demanding.matching.verifier_relaxed_min_margin = 1.0e6;
    let run = run_with(&scene, target("生", "なま", &[]), &demanding);
    assert!(matches(&run).is_empty());
}

#[test]
fn same_audio_gives_independent_decisions_per_target() {
    let scene = build(&[
        Seg::Silence(20),
        Seg::Word("s e e", "せい", -20.0),
        Seg::Silence(25),
        Seg::Word("n a m a", "なま", -20.0),
        Seg::Silence(20),
    ]);
    let nama = analyze(&scene, "生", "なま");
    let sei = analyze(&scene, "生", "せい");
    assert_eq!(matches(&nama)[0].frames, scene.truth[1]);
    assert_eq!(matches(&sei)[0].frames, scene.truth[0]);
    assert_ne!(
        target("生", "なま", &[]).matching_key(),
        target("生", "せい", &[]).matching_key()
    );
}

#[test]
fn contiguous_material_leaves_boundaries_unresolved() {
    // «せい» и «なま» без паузы: одно событие, тихого промежутка нет.
    let scene = build(&[
        Seg::Silence(20),
        Seg::Word("s e e", "せい", -20.0),
        Seg::Word("n a m a", "なま", -20.0),
        Seg::Silence(20),
    ]);
    let run = analyze(&scene, "生", "なま");
    assert_eq!(run.pre.events.len(), 1);
    assert_eq!(matches(&run).len(), 1, "{:#?}", run.matching.occurrences);
    assert_eq!(
        run.selection.outcome,
        RecognitionOutcome::BoundariesUnresolved
    );
    assert_eq!(
        action_for(run.selection.outcome, true),
        RecognitionAction::KeepFullRecord
    );
}

#[test]
fn single_word_whole_record_and_one_mora_word() {
    let scene = build(&[
        Seg::Silence(20),
        Seg::Word("n a m a", "なま", -20.0),
        Seg::Silence(20),
    ]);
    let run = analyze(&scene, "生", "なま");
    assert_eq!(run.selection.outcome, RecognitionOutcome::TargetWhole);
    let scene = build(&[
        Seg::Silence(15),
        Seg::Word("k i", "き", -20.0),
        Seg::Silence(15),
    ]);
    let run = analyze(&scene, "木", "き");
    assert_eq!(run.selection.outcome, RecognitionOutcome::TargetWhole);
}

#[test]
fn silence_and_noise_without_speech_are_no_speech() {
    let silence = build(&[Seg::Silence(60)]);
    let run_silence = analyze(&silence, "生", "なま");
    assert_eq!(run_silence.selection.outcome, RecognitionOutcome::NoSpeech);
    let noise = build(&[Seg::Silence(10), Seg::Noise(30, -25.0), Seg::Silence(10)]);
    let run_noise = analyze(&noise, "生", "なま");
    assert_eq!(run_noise.selection.outcome, RecognitionOutcome::NoSpeech);
    assert_eq!(run_noise.pre.unrecognized_activity.len(), 1);
}

#[test]
fn extra_noise_next_to_word_is_separated_or_unresolved_never_cut_into() {
    let scene = build(&[
        Seg::Silence(20),
        Seg::Word("n a m a", "なま", -20.0),
        Seg::Silence(15),
        Seg::Noise(10, -30.0),
        Seg::Silence(20),
    ]);
    let run = analyze(&scene, "生", "なま");
    assert_eq!(run.selection.outcome, RecognitionOutcome::TargetExtracted);
    let plan = run.selection.plan.unwrap();
    assert!(plan.extraction.contains(&span_of(scene.truth[0])));
    assert!(!plan.extraction.overlaps(&run.pre.unrecognized_activity[0]));
}

#[test]
fn minimal_pairs_cover_sokuon_length_voicing_and_palatals() {
    let p = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
    let pairs: Vec<String> = minimal_pairs(&p("by o o i N"))
        .into_iter()
        .map(|c| c.phonemes.join(" "))
        .collect();
    assert!(
        pairs.contains(&"by o i N".to_string()),
        "сокращение долготы"
    );
    assert!(pairs.contains(&"by o o i".to_string()), "без ん");
    assert!(pairs.contains(&"b i y o o i N".to_string()), "びよういん");
    let pairs: Vec<String> = minimal_pairs(&p("k i t e"))
        .into_iter()
        .map(|c| c.phonemes.join(" "))
        .collect();
    assert!(pairs.contains(&"k i cl t e".to_string()));
    assert!(pairs.contains(&"g i t e".to_string()));
}

#[test]
fn timeline_converts_between_rates_without_drift() {
    let t = Timeline {
        original_sha256: "0".repeat(64),
        sample_rate: 44_100,
        channels: 2,
        frames: 44_100,
        analysis_sample_rate: 16_000,
        analysis_frames: 16_000,
        analysis_downmix: "mean".into(),
        analysis_resampler: "swr".into(),
    };
    assert_eq!(t.from_analysis(16_000, false), 44_100);
    assert_eq!(t.from_analysis(1, false), 2);
    assert_eq!(t.from_analysis(1, true), 3);
    assert_eq!(t.from_analysis(-5, false), 0);
    assert_eq!(t.to_analysis(44_100), 16_000);
    assert!(t.from_analysis(20_000, true) <= t.frames);
}

#[test]
fn profile_validation_rejects_inconsistent_thresholds() {
    let good = RecognitionProfile::conservative();
    good.validate().unwrap();
    let mut bad = good.clone();
    bad.matching.match_min_score_per_token = 0.5;
    assert!(bad.validate().is_err());
    let mut bad = good.clone();
    bad.models.verifier = None;
    assert!(
        bad.validate().is_err(),
        "вторая модель обязательна по профилю"
    );
    let mut bad = good;
    bad.models.primary = "xlsr-japanese-hiragana-ctc".into();
    assert!(bad.validate().is_err(), "основная модель — фонемная");
}

#[test]
fn target_identity_requires_kana_alternatives_and_provenance() {
    target("生", "なま", &["せい"]).validate().unwrap();
    assert!(target("生", "なま", &["生"]).validate().is_err());
    assert!(target("生", "なま", &["なま"]).validate().is_err());
    let mut t = target("生", "なま", &[]);
    t.source.key.clear();
    assert!(t.validate().is_err());
}
