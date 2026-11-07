//! Локальный CPU-воркер нейросетевого улучшения речи для коротких клипов.
//!
//! Единственная модель этого crate — DeepFilterNet3 через `deep-filter-rt`
//! (Rust, `tract`, без Python и без нативных библиотек). Опциональный
//! воркер Resemble Enhance живёт отдельно (`resemble/`, Python) и в сборку
//! crate не входит. Веса проверяются по
//! закреплённому манифесту ([`model`]), инференс клипа — [`enhance`],
//! машинный протокол процесса — [`protocol`].

pub mod enhance;
pub mod model;
pub mod protocol;
