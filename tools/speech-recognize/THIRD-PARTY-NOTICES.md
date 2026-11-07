# Сторонние компоненты speech-recognize

Код этого пакета распространяется под AGPL-3.0-only проекта. Перечисленные ниже
компоненты сохраняют собственные лицензии. Веса моделей **не входят** в
репозиторий и в сборку воркера: их явно устанавливает пользователь
(`pronunciation-assets recognizer model-install`) из локального снимка, а воркер
при каждом запуске сверяет длины и SHA-256 файлов с закреплёнными в
`src/model.rs`.

## japanese-hubert-base-phoneme-ctc-v4 (основная фонемная модель)

- Источник: <https://huggingface.co/prj-beatrice/japanese-hubert-base-phoneme-ctc-v4>,
  ревизия `f5fe07043bcb0b77a86faf72ac6d8fc1ae558f99`.
- Лицензия весов: Apache-2.0 (по карточке модели).
- Происхождение: дообучение `rinna/japanese-hubert-base` (Apache-2.0) на CTC
  распознавание фонем pyopenjtalk; данные обучения по карточке —
  `reazon-research/reazonspeech`. Условия использования набора данных задают
  его правообладатели.

## wav2vec2-large-xlsr-japanese-hiragana (независимая модель второй гипотезы)

- Источник: <https://huggingface.co/vumichien/wav2vec2-large-xlsr-japanese-hiragana>,
  ревизия `017225bb128a6b1c6de9d58391f891908d69bc7b`.
- Лицензия весов: Apache-2.0 (по карточке модели).
- Происхождение: дообучение `facebook/wav2vec2-large-xlsr-53` (Apache-2.0) на
  Common Voice (японский) и корпусе JSUT по карточке модели. Условия
  использования наборов данных задают их правообладатели; у JSUT они
  ограничивают коммерческое использование корпуса, и решение о применимости
  к своему случаю принимает пользователь, устанавливающий веса.

## Архитектура и эталон проверки

Реализация прямого прохода HuBERT/wav2vec2 написана заново на Rust по
описанию архитектуры; код Hugging Face `transformers` (Apache-2.0) в пакет не
входит. Им пользуется только генератор тестовых фикстур
`tests/fixtures/generate.py`: крошечные модели фикстур созданы со случайными
весами и чужих весов не содержат.

## Зависимости Rust

`ndarray`, `matrixmultiply`, `rustix`, `serde`, `serde_json`, `sha2`, `clap` —
MIT OR Apache-2.0 (для `rustix` также Apache-2.0 WITH LLVM-exception);
политику лицензий зависимостей проверяет `deny.toml`.
