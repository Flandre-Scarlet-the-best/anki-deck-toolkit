//! Эквивалентность прямого прохода с эталонной реализацией transformers на
//! крошечных моделях со случайными весами (фикстуры `tests/fixtures`,
//! генератор — `tests/fixtures/generate.py`). Настоящие веса для этих
//! тестов не нужны; сверка с ними описана в `docs/ru/asset-store/recognizer.md`.

use std::collections::BTreeMap;
use std::path::Path;

use speech_recognize::encoder::{Architecture, Model, Spec};
use speech_recognize::safetensors::{self, Tensor};

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
