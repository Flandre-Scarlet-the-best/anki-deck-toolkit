//! Интеллектуальный выбор аудиокандидатов и адресная проверка.
//!
//! Четыре раздельные сущности:
//!
//! 1. **Исходный ассет** ([`AudioSource`]) — неизменяемые байты MP3, SHA,
//!    происхождение и проверка при приёме. Чтение и потребитель не нужны.
//! 2. **Кандидат произношения** ([`PronunciationCandidate`] внутри
//!    [`TargetLink`]) — интерпретация исходника или его точного фрагмента как
//!    конкретной пары `surface + reading` со свидетельствами распознавателя.
//! 3. **Решение** ([`SelectionDecision`]) — какой кандидат выбран, кем
//!    ([`DecisionAuthority`]) и почему. Это не `HumanAttestation` и не меняет
//!    доверие канонического `pronunciation_audio`.
//! 4. **Привязка к потребителю** — вне этого модуля: потребитель запрашивает
//!    аудио по паре через [`read_selected`] с явной [`ConsumerTrustPolicy`]
//!    или через прежний `read_trusted` после явного [`promote`].
//!
//! Состояние живёт в `SafeBatchRuntime` хранилища произношений
//! (`.runtime/batches/<batch-id>/`), байты — в его неизменяемых blob.

mod engine;
pub mod model;
pub mod ops;
pub mod phonetic;
pub mod review;

pub use model::*;
pub use ops::{
    AgentPolicy, AutoSelectReport, ConsumerTrustPolicy, DecideAction, DecideReport, DecideRequest,
    ExportedAudio, GroupFilter, IntakeFile, IntakeReport, LinkReport, PERCEPTION_NOTICE,
    PromoteReport, SelectedAudio, SelectedAudioInfo, auto_select, decide, export_selected,
    intake_files, intake_forvo_batch, intake_paths, link_recognizer, link_target, promote,
    read_selected, read_selected_info, status_value,
};

#[cfg(test)]
mod tests;
