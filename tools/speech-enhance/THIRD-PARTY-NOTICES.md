# Сторонние компоненты speech-enhance

Код этого пакета распространяется под AGPL-3.0-only проекта. Перечисленные ниже
компоненты сохраняют собственные лицензии.

## DeepFilterNet (код инференса и модель DeepFilterNet3)

- Источник: <https://github.com/Rikorose/DeepFilterNet>, коммит
  `d375b2d8309e0935d165700c91da9de862a99c31`, архив
  `models/DeepFilterNet3_onnx.tar.gz` (SHA-256
  `c94d91f70911001c946e0fabb4aa9adc37045f45a03b56008cb0c8244cb63616`).
- Автор: Hendrik Schröter.
- Лицензия: MIT OR Apache-2.0 (по README upstream-репозитория).
- Использование: веса **не входят** в репозиторий и в сборку воркера. Их
  явно устанавливает пользователь (`enhancer model-install`), воркер читает их
  из каталога и сверяет по закреплённым SHA-256 (`src/model.rs`).

## deep-filter-rt 0.2.0

- Источник: crates.io, crate `deep-filter-rt` (инференсная часть `deep_filter`
  из DeepFilterNet, сопровождение Thomas Heck; `NOTICE.md` crate описывает
  происхождение и изменения).
- Лицензия: MIT OR Apache-2.0.
- В пакете crate есть копия тех же весов DeepFilterNet3; воркер её не
  использует, и в исполняемый файл она не попадает.

## tract (Sonos)

- Источник: crates.io (`tract-core`, `tract-onnx`, `tract-pulse` и зависимости).
- Лицензия: MIT OR Apache-2.0.

## Resemble Enhance (экспериментальный опциональный воркер `resemble/`)

- Код: <https://github.com/resemble-ai/resemble-enhance>, пакет PyPI
  `resemble-enhance` 0.0.1, © 2023 Resemble AI, лицензия MIT.
- Веса: <https://huggingface.co/ResembleAI/resemble-enhance>, ревизия
  `4e3510ce4a8391159f665903544c5150bee7b2cb`, лицензия MIT по карточке модели.
  Используется только denoiser из чекпойнта `enhancer_stage2`.
- Использование: ни код пакета, ни веса **не входят** в репозиторий. Окружение
  (`resemble/requirements.txt`) и веса пользователь ставит сам. Воркер сверяет
  веса по закреплённым SHA-256 до загрузки (`resemble/speech_enhance_resemble.py`).
- Зависимости окружения сохраняют свои лицензии, в том числе PyTorch и
  torchaudio (BSD-3-Clause) и библиотеки NVIDIA CUDA из колёс PyPI (собственные
  условия NVIDIA).

Полные тексты лицензий MIT и Apache-2.0 поставляются в пакетах соответствующих
crate в реестре Cargo, тексты лицензий Python-пакетов — в их каталогах
`*.dist-info` в окружении пользователя.
