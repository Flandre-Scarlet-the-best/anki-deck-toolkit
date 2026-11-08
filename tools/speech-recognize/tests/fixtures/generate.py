#!/usr/bin/env python3
"""Генератор фикстур эквивалентности прямого прохода с transformers.

Строит две крошечные модели со случайными весами (фиксированный seed) —
HuBERT с GroupNorm и post-LN энкодером и wav2vec2 со stable layer norm — и
сохраняет веса (safetensors), вход и эталонные логарифмы вероятностей
transformers. Тест `tests/engine.rs` сверяет с ними Rust-реализацию.
Нужен только для пересборки фикстур (torch, transformers, safetensors):

    python3 generate.py
"""

import json
from pathlib import Path

import numpy as np
import torch
from transformers import HubertConfig, HubertForCTC, Wav2Vec2Config, Wav2Vec2ForCTC

HERE = Path(__file__).resolve().parent
COMMON = dict(
    conv_dim=[8] * 7,
    conv_kernel=[10, 3, 3, 3, 3, 2, 2],
    conv_stride=[5, 2, 2, 2, 2, 2, 2],
    hidden_size=16,
    num_hidden_layers=2,
    num_attention_heads=2,
    intermediate_size=32,
    num_conv_pos_embeddings=16,
    num_conv_pos_embedding_groups=4,
    vocab_size=6,
    hidden_act="gelu",
    feat_extract_activation="gelu",
    layer_norm_eps=1e-5,
)


def build(name, cls, config):
    torch.manual_seed(7)
    model = cls(config).eval()
    with torch.no_grad():
        for p in model.parameters():
            p.copy_(torch.randn_like(p) * 0.3)
    rng = np.random.default_rng(11)
    samples = (rng.standard_normal(3200) * 0.2).astype(np.float32)
    with torch.no_grad():
        logits = model(torch.from_numpy(samples)[None]).logits[0]
        expected = torch.log_softmax(logits, -1).numpy().astype(np.float32)
    model.save_pretrained(HERE / name, safe_serialization=True)
    for extra in ("config.json", "generation_config.json"):
        (HERE / name / extra).unlink(missing_ok=True)
    samples.tofile(HERE / name / "input.f32")
    expected.tofile(HERE / name / "expected.f32")
    (HERE / name / "shape.json").write_text(json.dumps(list(expected.shape)) + "\n")


build(
    "tiny-hubert",
    HubertForCTC,
    HubertConfig(**COMMON, feat_extract_norm="group", conv_bias=False, do_stable_layer_norm=False,
                 feat_proj_layer_norm=True),
)
build(
    "tiny-wav2vec2-stable",
    Wav2Vec2ForCTC,
    Wav2Vec2Config(**COMMON, feat_extract_norm="layer", conv_bias=True, do_stable_layer_norm=True),
)
