//! Эквивалентность прямого прохода с эталонной реализацией transformers на
//! крошечных моделях со случайными весами (фикстуры `tests/fixtures`,
//! генератор — `tests/fixtures/generate.py`). Настоящие веса для этих
//! тестов не нужны; сверка с ними описана в `docs/ru/asset-store/recognizer.md`.

use std::collections::BTreeMap;
use std::path::Path;

use speech_recognize::encoder::{Architecture, Model, Spec};
use speech_recognize::model::{ModelIdentity, OutputKind};
use speech_recognize::safetensors::{self, Tensor};
use speech_recognize::{infer, protocol};

/// Веса, вход, ожидаемые логиты и форма `кадры × словарь`.
type Fixture = (BTreeMap<String, Tensor>, Vec<f32>, Vec<f32>, (usize, usize));

fn fixture(name: &str) -> Fixture {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let floats = |file: &str| -> Vec<f32> {
        std::fs::read(dir.join(file))
            .unwrap()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect()
    };
    let tensors =
        safetensors::parse(&std::fs::read(dir.join("model.safetensors")).unwrap()).unwrap();
    let shape: [usize; 2] =
        serde_json::from_slice(&std::fs::read(dir.join("shape.json")).unwrap()).unwrap();
    (
        tensors,
        floats("input.f32"),
        floats("expected.f32"),
        (shape[0], shape[1]),
    )
}

fn tiny_spec(architecture: Architecture, prefix: &str, conv_bias: bool) -> Spec {
    Spec {
        architecture,
        prefix: prefix.into(),
        conv_dim: 8,
        conv_kernel: vec![10, 3, 3, 3, 3, 2, 2],
        conv_stride: vec![5, 2, 2, 2, 2, 2, 2],
        conv_bias,
        hidden: 16,
        layers: 2,
        heads: 2,
        intermediate: 32,
        pos_kernel: 16,
        pos_groups: 4,
        vocab: 6,
    }
}

fn assert_close(actual: &[f32], expected: &[f32]) {
    assert_eq!(actual.len(), expected.len());
    let worst = actual
        .iter()
        .zip(expected)
        .map(|(a, e)| (a - e).abs())
        .fold(0.0f32, f32::max);
    assert!(worst < 1e-4, "наибольшее расхождение log-prob {worst}");
}

#[test]
fn hubert_post_layer_norm_matches_transformers() {
    let (tensors, input, expected, shape) = fixture("tiny-hubert");
    let model = Model::build(
        tiny_spec(Architecture::PostLayerNorm, "hubert", false),
        tensors,
    )
    .unwrap();
    let out = model.log_probs(&input);
    assert_eq!(out.dim(), shape);
    assert_close(out.as_slice().unwrap(), &expected);
}

#[test]
fn wav2vec2_stable_layer_norm_matches_transformers() {
    let (tensors, input, expected, shape) = fixture("tiny-wav2vec2-stable");
    let model = Model::build(
        tiny_spec(Architecture::StableLayerNorm, "wav2vec2", true),
        tensors,
    )
    .unwrap();
    let out = model.log_probs(&input);
    assert_eq!(out.dim(), shape);
    assert_close(out.as_slice().unwrap(), &expected);
}

/// Старые чекпойнты (в том числе закреплённый XLSR) хранят weight norm как
/// `weight_g`/`weight_v`: тот же тензор под старыми именами даёт тот же выход.
#[test]
fn legacy_weight_norm_names_are_equivalent() {
    let (mut tensors, input, expected, _) = fixture("tiny-wav2vec2-stable");
    let base = "wav2vec2.encoder.pos_conv_embed.conv";
    let g = tensors
        .remove(&format!("{base}.parametrizations.weight.original0"))
        .unwrap();
    let v = tensors
        .remove(&format!("{base}.parametrizations.weight.original1"))
        .unwrap();
    tensors.insert(format!("{base}.weight_g"), g);
    tensors.insert(format!("{base}.weight_v"), v);
    let model = Model::build(
        tiny_spec(Architecture::StableLayerNorm, "wav2vec2", true),
        tensors,
    )
    .unwrap();
    assert_close(model.log_probs(&input).as_slice().unwrap(), &expected);
}

#[test]
fn wrong_architecture_or_extra_tensors_are_rejected() {
    let (tensors, _, _, _) = fixture("tiny-hubert");
    // Чекпойнт HuBERT не собирается как stable-LN (нет LayerNorm у свёрток 1…6).
    assert!(
        Model::build(
            tiny_spec(Architecture::StableLayerNorm, "hubert", false),
            tensors.clone()
        )
        .is_err()
    );
    let mut extra = tensors.clone();
    extra.insert(
        "unexpected".into(),
        Tensor {
            shape: vec![1],
            data: vec![0.0],
        },
    );
    let err = Model::build(
        tiny_spec(Architecture::PostLayerNorm, "hubert", false),
        extra,
    )
    .err()
    .unwrap();
    assert!(err.0.contains("unexpected"), "{err}");
    let mut wrong_shape = tensors;
    wrong_shape.get_mut("lm_head.bias").unwrap().shape = vec![7];
    assert!(
        Model::build(
            tiny_spec(Architecture::PostLayerNorm, "hubert", false),
            wrong_shape
        )
        .is_err()
    );
}

#[test]
fn short_input_gives_no_frames_and_output_is_deterministic() {
    let (tensors, input, _, _) = fixture("tiny-hubert");
    let model = Model::build(
        tiny_spec(Architecture::PostLayerNorm, "hubert", false),
        tensors,
    )
    .unwrap();
    assert_eq!(model.log_probs(&input[..399]).dim(), (0, 6));
    let a = model.log_probs(&input);
    let b = model.log_probs(&input);
    assert_eq!(a, b);
}

fn tiny_identity(lead: usize, trail: usize, normalize: bool) -> ModelIdentity {
    ModelIdentity {
        id: "tiny".into(),
        name: "tiny".into(),
        repository: String::new(),
        revision: String::new(),
        license: String::new(),
        output: OutputKind::Phoneme,
        manifest_sha256: String::new(),
        files: Vec::new(),
        sample_rate: 16_000,
        frame_stride: 320,
        receptive_field: 400,
        lead_pad_samples: lead,
        trail_pad_samples: trail,
        normalize_input: normalize,
        blank_id: 0,
        vocab: (0..6).map(|i| i.to_string()).collect(),
    }
}

#[test]
fn input_is_padded_and_normalised_by_the_manifest() {
    let samples = [1.0f32, 3.0];
    let plain = infer::prepare_input(&tiny_identity(2, 1, false), &samples);
    assert_eq!(plain, [0.0, 0.0, 1.0, 3.0, 0.0]);
    // Нормализация считается по всему дополненному сигналу.
    let normalised = infer::prepare_input(&tiny_identity(2, 1, true), &samples);
    let n = normalised.len() as f64;
    let mean = normalised.iter().map(|v| f64::from(*v)).sum::<f64>() / n;
    let var = normalised
        .iter()
        .map(|v| (f64::from(*v) - mean).powi(2))
        .sum::<f64>()
        / n;
    assert!(
        mean.abs() < 1e-6 && (var - 1.0).abs() < 1e-5,
        "{mean} {var}"
    );
    assert!(infer::prepare_input(&tiny_identity(0, 0, true), &[]).is_empty());
}

#[test]
fn emissions_are_log_probabilities_of_the_padded_input() {
    let (tensors, input, expected, shape) = fixture("tiny-hubert");
    let model = Model::build(
        tiny_spec(Architecture::PostLayerNorm, "hubert", false),
        tensors,
    )
    .unwrap();
    // Без дополнения эмиссии — тот же прямой проход, что и в эталоне.
    let e = infer::emissions(&model, &tiny_identity(0, 0, false), &input);
    assert_eq!((e.frames, e.vocab), shape);
    assert_close(&e.log_probs, &expected);
    // Дополнение тишиной добавляет кадры, каждая строка — распределение.
    let padded = infer::emissions(&model, &tiny_identity(800, 800, false), &input);
    assert!(padded.frames > e.frames);
    for row in padded.log_probs.chunks(padded.vocab) {
        let total: f32 = row.iter().map(|v| v.exp()).sum();
        assert!((total - 1.0).abs() < 1e-4, "{total}");
    }
}

#[test]
fn protocol_reports_worker_and_bounded_threads() {
    let worker = protocol::worker_info();
    assert_eq!(worker.name, protocol::WORKER_NAME);
    assert_eq!(worker.protocol, protocol::PROTOCOL_VERSION);
    let runtime = protocol::runtime_info();
    assert_eq!(runtime.device, "cpu");
    assert!((1..=4).contains(&runtime.threads));
}
