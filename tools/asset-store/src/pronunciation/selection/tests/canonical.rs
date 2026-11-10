//! Готовые MP3 конкретных чтений: материализация выбранного интервала,
//! согласованность готовности (`status`, `unresolved`, `read`, `export`,
//! `promote`), имена `<surface>_<reading>.mp3`, публикация без замены чужих
//! файлов и режим проверки пакета.

use super::*;
use crate::pronunciation::selection::materialize;
use crate::pronunciation::selection::ops::{
    AFTER_PUBLISH, BEFORE_PUBLISH, Readiness, ReviewModeRequest, persist_selected_file_for_test,
    set_review_mode,
};

/// Запись `生` из двух различимых частей: 440 Гц (0–500 мс), тишина,
/// 880 Гц (700–1300 мс). Части играют роль двух чтений одной записи.
fn two_part_mp3() -> Vec<u8> {
    let workspace = TempWorkspace::create("selection-two-part").unwrap();
    let path = workspace.path().join("fixture.mp3");
    let status = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-f", "lavfi", "-i"])
        .arg(
            "aevalsrc='if(lt(t,0.5),0.5*sin(2*PI*440*t),if(lt(t,0.7),0,0.5*sin(2*PI*880*t)))':s=44100:d=1.3",
        )
        .args(["-c:a", "libmp3lame", "-q:a", "4"])
        .arg(&path)
        .status()
        .expect("для тестов нужен FFmpeg");
    assert!(status.success());
    std::fs::read(path).unwrap()
}

/// Частота переходов через ноль: грубая оценка основной частоты тона.
fn dominant_frequency(bytes: &[u8]) -> f64 {
    let probe = probe_mp3(bytes).unwrap();
    let samples = crate::pronunciation::probe::decode_f32(bytes, &probe).unwrap();
    let crossings = samples
        .windows(2)
        .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
        .count();
    crossings as f64 / 2.0 / probe.decoded_duration_seconds
}

struct TwoReadings {
    _workspace: TempWorkspace,
    store: AssetStore,
    source_sha: String,
    sei: TargetLink,
    nama: TargetLink,
}

/// Одна запись `生`, две связи (`せい` — первая часть, `なま` — вторая),
/// у каждой — кандидаты-интервалы без байтов.
fn two_readings() -> TwoReadings {
    let (workspace, store) = store();
    let intake = intake_files(
        &store,
        "sel",
        vec![IntakeFile {
            source_name: "生.mp3".into(),
            bytes: two_part_mp3(),
        }],
        Some("生"),
    )
    .unwrap();
    let source_sha = intake.added[0].clone();
    let mut links = Vec::new();
    mutate(&store, "sel", |batch, _| {
        let source = batch.source(&source_sha).unwrap().clone();
        let sei = multi_reading_link(
            &source,
            "せい",
            RecognitionOutcome::MultipleOccurrences,
            &[(40.0, 460.0)],
        );
        let nama = multi_reading_link(
            &source,
            "なま",
            RecognitionOutcome::MultipleOccurrences,
            &[(40.0, 460.0), (760.0, 1260.0)],
        );
        batch.links.push(sei.clone());
        batch.links.push(nama.clone());
        links.push(sei);
        links.push(nama);
    });
    let nama = links.pop().unwrap();
    let sei = links.pop().unwrap();
    TwoReadings {
        _workspace: workspace,
        store,
        source_sha,
        sei,
        nama,
    }
}

fn interval(link: &TargetLink, index: usize) -> &PronunciationCandidate {
    link.candidates
        .iter()
        .filter(|c| c.kind == CandidateKind::OccurrenceInterval)
        .nth(index)
        .unwrap()
}

fn select_by_user(
    store: &AssetStore,
    reading: &str,
    candidate: &PronunciationCandidate,
    sha: &str,
) {
    decide(
        store,
        "sel",
        DecideRequest {
            surface: "生".into(),
            reading: reading.into(),
            action: DecideAction::Select {
                candidate_id: candidate.candidate_id.clone(),
                expected_sha256: sha.into(),
            },
            authority: user(&format!("выберите вариант {reading}"), true),
            reason: "ответ пользователя".into(),
            expected_fingerprint: None,
        },
    )
    .unwrap();
}

fn group<'a>(status: &'a serde_json::Value, reading: &str) -> Option<&'a serde_json::Value> {
    status["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["reading"] == reading)
}

fn refusal_code(error: &crate::AssetError) -> &str {
    error.details["code"].as_str().unwrap_or_default()
}

#[test]
fn consumer_names_include_reading_and_never_collide() {
    let sei = Pronunciation::new("生", "せい").unwrap();
    let nama = Pronunciation::new("生", "なま").unwrap();
    assert_eq!(sei.consumer_filename(), "生_せい.mp3");
    assert_eq!(nama.consumer_filename(), "生_なま.mp3");
    assert_eq!(sei.legacy_consumer_filename(), "生.mp3");
    // `_` и символы пути в чтении сделали бы имя неоднозначным.
    for reading in ["な_ま", "なま/", "なま:", "なま."] {
        assert!(Pronunciation::new("生", reading).is_err(), "{reading}");
        // Историческая идентичность с таким чтением допустима и не блокирует
        // хранилище, но её чтение в имя не входит.
        let identity = crate::AssetIdentity {
            namespace: "pronunciation_audio".into(),
            key: serde_json::to_string(&["生", reading]).unwrap(),
        };
        let historical = Pronunciation::from_identity(&identity).unwrap();
        assert_eq!(historical.consumer_filename(), "生.mp3", "{reading}");
    }
    // Историческое имя допустимо только у своей идентичности.
    use crate::AssetDomainPolicy;
    let policy = PronunciationDomainPolicy;
    assert!(policy.record_consumer_filename_matches(&sei.identity(), "生.mp3", "生_せい.mp3"));
    assert!(policy.record_consumer_filename_matches(&sei.identity(), "生_せい.mp3", "生_せい.mp3"));
    assert!(!policy.record_consumer_filename_matches(
        &sei.identity(),
        "生_なま.mp3",
        "生_せい.mp3"
    ));
    assert!(!policy.record_consumer_filename_matches(&sei.identity(), "本.mp3", "生_せい.mp3"));
    assert!(policy.proves_legacy_consumer_filename(&sei.identity(), "生.mp3"));
    // Имена, различающиеся только регистром или Unicode-формой: ключ сравнения
    // сворачивает их, и набор с такими парами отклоняется до выдачи.
    let upper = Pronunciation::new("ABC", "えー").unwrap();
    let lower = Pronunciation::new("abc", "えー").unwrap();
    assert_eq!(
        policy.consumer_name_key(&upper.consumer_filename()),
        policy.consumer_name_key(&lower.consumer_filename())
    );
}

/// Повторное связывание сохраняет материализованного кандидата, даже если
/// новый пакет распознавания пронумеровал вхождения иначе: номер вхождения
/// берётся из нового интервала, и пакет остаётся валидным.
#[test]
fn relinking_carries_materialized_candidate_with_renumbered_occurrence() {
    let f = two_readings();
    let second = interval(&f.nama, 1).clone();
    select_by_user(&f.store, "なま", &second, &f.source_sha);
    let report = materialize::materialize(&f.store, "sel", None).unwrap();
    let produced = report.materialized[0].candidate_id.clone();

    // Новое распознавание нашло лишнее вхождение раньше: те же границы
    // получают номера на единицу больше, идентификаторы интервалов не меняются.
    let mut fresh = f.nama.clone();
    for occurrence in &mut fresh.occurrences {
        occurrence.index += 1;
    }
    for candidate in &mut fresh.candidates {
        candidate.occurrence = candidate.occurrence.map(|i| i + 1);
    }
    let previous = load(&f.store, "sel")
        .links
        .into_iter()
        .find(|l| l.link_id == f.nama.link_id)
        .unwrap();
    let kept = ops::carried_materialized(&previous.candidates, &fresh.candidates);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].candidate_id, produced);
    assert_eq!(kept[0].occurrence, interval(&fresh, 1).occurrence);
    assert_ne!(kept[0].occurrence, second.occurrence);
    fresh.candidates.extend(kept);
    mutate(&f.store, "sel", |batch, _| {
        let link = batch
            .links
            .iter_mut()
            .find(|l| l.link_id == fresh.link_id)
            .unwrap();
        *link = fresh;
    });
}

/// Выбранный интервал — не готовый результат: до материализации цель не
/// выдаётся ни одним путём, после — выдаёт именно нужную часть записи.
#[test]
fn selected_interval_becomes_consumable_only_after_materialization() {
    let f = two_readings();
    let second = interval(&f.nama, 1).clone();
    select_by_user(&f.store, "なま", &second, &f.source_sha);

    let batch = load(&f.store, "sel");
    let index = engine::SelectionIndex::new(&batch);
    let decision = batch.current_decision("生", "なま").unwrap();
    assert!(ops::decision_is_effective(&index, &batch, decision));
    let status = ops::status_value(&f.store, "sel", GroupFilter::Unresolved, 50).unwrap();
    let nama = group(&status, "なま").expect("выбранный интервал остаётся нерешённым");
    assert_eq!(nama["selection_effective"], true);
    assert_eq!(nama["readiness"]["state"], "pending_materialization");
    assert_eq!(
        status["dispositions"]["selected_pending_materialization"],
        1
    );
    assert!(status["decided"].get("user_via_agent").is_none());
    let requests = [Pronunciation::new("生", "なま").unwrap()];
    let policy = ConsumerTrustPolicy::HumanDecision;
    let error = read_selected(f.store.root(), "sel", &requests, policy).unwrap_err();
    assert_eq!(refusal_code(&error), "interval_not_materialized");
    let out = f._workspace.path().join("out");
    let error = export_selected(f.store.root(), "sel", &requests, policy, &out).unwrap_err();
    assert_eq!(refusal_code(&error), "interval_not_materialized");
    assert!(!out.exists());
    let error = promote(&f.store, "sel", "生", "なま", None, false, policy).unwrap_err();
    assert_eq!(refusal_code(&error), "interval_not_materialized");
    let auto = auto_select(&f.store, "sel", None).unwrap();
    assert_eq!(
        auto.not_ready["selected_pending_materialization"],
        vec![("生".to_owned(), "なま".to_owned())]
    );

    let report = materialize::materialize(&f.store, "sel", None).unwrap();
    assert_eq!(report.materialized.len(), 1);
    assert!(report.failed.is_empty());
    let produced = &report.materialized[0];
    assert_eq!(produced.interval_candidate_id, second.candidate_id);
    // Повтор не создаёт второго кандидата для тех же границ.
    let again = materialize::materialize(&f.store, "sel", None).unwrap();
    assert!(again.materialized.is_empty());
    assert_eq!(again.already_materialized, 1);

    let status = ops::status_value(&f.store, "sel", GroupFilter::All, 50).unwrap();
    let nama = group(&status, "なま").unwrap();
    assert_eq!(nama["readiness"]["state"], "ready");
    assert_eq!(nama["readiness"]["sha256"], produced.sha256.as_str());
    assert_eq!(status["decided"]["user_via_agent"], 1);
    let unresolved = ops::status_value(&f.store, "sel", GroupFilter::Unresolved, 50).unwrap();
    assert!(group(&unresolved, "なま").is_none());

    let selected = read_selected(f.store.root(), "sel", &requests, policy).unwrap();
    assert_eq!(selected[0].info.kind, CandidateKind::MaterializedInterval);
    assert_eq!(selected[0].info.candidate_id, produced.candidate_id);
    assert_eq!(selected[0].info.decision_candidate_id, second.candidate_id);
    assert_eq!(sha256_hex(&selected[0].bytes), produced.sha256);
    assert_ne!(produced.sha256, f.source_sha);
    let probe = probe_mp3(&selected[0].bytes).unwrap();
    assert!((probe.decoded_duration_seconds - 0.5).abs() < 0.06);
    let frequency = dominant_frequency(&selected[0].bytes);
    assert!(
        (frequency - 880.0).abs() < 60.0,
        "выдана вторая часть записи, а не первая: {frequency}"
    );

    // Цепочка обработки записана у кандидата и не теряет исходник.
    let batch = load(&f.store, "sel");
    let (link, materialized) = batch.find_candidate(&produced.candidate_id).unwrap();
    let derivation = materialized.derivation.as_ref().unwrap();
    assert_eq!(derivation.source_sha256, f.source_sha);
    assert_eq!(derivation.from_candidate_id, second.candidate_id);
    assert_eq!(Some(derivation.span), second.span);
    assert_eq!(derivation.transform, materialize::TRANSFORM);
    assert!(
        derivation
            .toolchain
            .iter()
            .any(|t| t.starts_with("ffmpeg:"))
    );
    assert!(batch.source(&link.source_sha256).is_some());
    assert!(matches!(
        batch
            .materialization_of(&second.candidate_id)
            .unwrap()
            .outcome,
        MaterializationOutcome::Materialized { .. }
    ));

    // Каноническая публикация производного MP3 с полным свидетельством; она
    // не заявляет прослушивания полученных байтов.
    let refused = promote(&f.store, "sel", "生", "なま", None, true, policy).unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidValidationEvidence);
    let promoted = promote(&f.store, "sel", "生", "なま", None, false, policy).unwrap();
    assert_eq!(promoted.kind, CandidateKind::MaterializedInterval);
    assert_eq!(promoted.asset.consumer_filename, "生_なま.mp3");
    assert_eq!(promoted.asset.sha256, produced.sha256);
    assert_eq!(
        promoted.asset.provenance.source_kind,
        "selection_derivative"
    );
    assert_ne!(promoted.asset.lifecycle, LifecycleState::Verified);
    let metadata = &promoted.asset.domain_metadata.as_ref().unwrap()["selection_derivative"];
    assert_eq!(metadata["source_sha256"], f.source_sha.as_str());
    assert_eq!(
        metadata["span"]["start_frame"],
        second.span.unwrap().start_frame
    );
    assert_eq!(metadata["transform"], materialize::TRANSFORM);
    assert_eq!(metadata["selection"]["authority"], "user_via_agent");
}

/// Из одной записи с двумя чтениями получаются два самостоятельных MP3 под
/// разными именами; оригинал остаётся в пакете.
#[test]
fn one_recording_yields_independent_mp3_for_each_reading() {
    let f = two_readings();
    select_by_user(&f.store, "せい", interval(&f.sei, 0), &f.source_sha);
    select_by_user(&f.store, "なま", interval(&f.nama, 1), &f.source_sha);
    let report = materialize::materialize(&f.store, "sel", None).unwrap();
    assert_eq!(report.materialized.len(), 2);
    let requests = [
        Pronunciation::new("生", "せい").unwrap(),
        Pronunciation::new("生", "なま").unwrap(),
    ];
    let out = f._workspace.path().join("media");
    let exported = export_selected(
        f.store.root(),
        "sel",
        &requests,
        ConsumerTrustPolicy::HumanDecision,
        &out,
    )
    .unwrap();
    let names: Vec<_> = exported
        .iter()
        .map(|e| e.consumer_filename.as_str())
        .collect();
    assert_eq!(names, ["生_せい.mp3", "生_なま.mp3"]);
    assert_ne!(exported[0].sha256, exported[1].sha256);
    let sei = std::fs::read(out.join("生_せい.mp3")).unwrap();
    let nama = std::fs::read(out.join("生_なま.mp3")).unwrap();
    assert!((dominant_frequency(&sei) - 440.0).abs() < 40.0);
    assert!((dominant_frequency(&nama) - 880.0).abs() < 60.0);
    for (file, item) in [(&sei, &exported[0]), (&nama, &exported[1])] {
        assert_eq!(&sha256_hex(file), &item.sha256);
    }
    let batch = load(&f.store, "sel");
    let source = batch.source(&f.source_sha).unwrap();
    let runtime = SafeBatchRuntime::open(f.store.root(), "sel").unwrap();
    let original = runtime
        .read_blob_with_limit(&source.blob, crate::pronunciation::MAX_AUDIO_BYTES)
        .unwrap();
    assert_eq!(sha256_hex(&original), f.source_sha, "оригинал не удалён");
    drop(runtime);
    for reading in ["せい", "なま"] {
        promote(
            &f.store,
            "sel",
            "生",
            reading,
            None,
            false,
            ConsumerTrustPolicy::HumanDecision,
        )
        .unwrap();
    }
}

/// Неудачная материализация оставляет цель в явном отказе; противоречащая
/// экспертиза отзывает готовность; исходник и журнал сохраняются.
#[test]
fn failed_or_contradicted_materialization_revokes_readiness_without_losing_evidence() {
    let f = two_readings();
    // Интервал за пределами записи: материализация обязана отказать.
    let mut outside = None;
    mutate(&f.store, "sel", |batch, _| {
        let link = batch
            .links
            .iter_mut()
            .find(|l| l.reading == "せい")
            .unwrap();
        let mut late = interval(link, 0).clone();
        late.span = Some(span(1250.0, 1900.0));
        late.candidate_id = "c-outside".into();
        link.candidates.push(late.clone());
        outside = Some(late);
    });
    let outside = outside.unwrap();
    select_by_user(&f.store, "せい", &outside, &f.source_sha);
    select_by_user(&f.store, "なま", interval(&f.nama, 1), &f.source_sha);
    let report = materialize::materialize(&f.store, "sel", None).unwrap();
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].code, "interval_out_of_range");
    assert_eq!(report.materialized.len(), 1);
    let status = ops::status_value(&f.store, "sel", GroupFilter::Unresolved, 50).unwrap();
    let sei = group(&status, "せい").expect("отказ остаётся нерешённым");
    assert_eq!(sei["readiness"]["state"], "materialization_failed");
    assert_eq!(sei["readiness"]["code"], "interval_out_of_range");
    let requests = [Pronunciation::new("生", "せい").unwrap()];
    let error = read_selected(
        f.store.root(),
        "sel",
        &requests,
        ConsumerTrustPolicy::HumanDecision,
    )
    .unwrap_err();
    assert_eq!(refusal_code(&error), "materialization_failed");
    let batch = load(&f.store, "sel");
    assert!(batch.source(&f.source_sha).is_some());
    assert_eq!(batch.materializations.len(), 2);

    // Противоречащая экспертиза полученных байтов отзывает готовность даже
    // для решения пользователя.
    let produced = report.materialized[0].clone();
    mutate(&f.store, "sel", |batch, _| {
        let (_, candidate) = batch.find_candidate(&produced.candidate_id).unwrap();
        let mut review = audio_review(candidate, &produced.sha256, AudioVerdict::Contradicts);
        review.surface = "生".into();
        review.reading = "なま".into();
        review.competitors = vec!["せい".into()];
        batch.audio_reviews.push(review);
    });
    let requests = [Pronunciation::new("生", "なま").unwrap()];
    let error = read_selected(
        f.store.root(),
        "sel",
        &requests,
        ConsumerTrustPolicy::HumanDecision,
    )
    .unwrap_err();
    assert_eq!(refusal_code(&error), "semantic_contradicted");
    let status = ops::status_value(&f.store, "sel", GroupFilter::Unresolved, 50).unwrap();
    assert_eq!(
        group(&status, "なま").unwrap()["readiness"]["state"],
        "semantic_contradicted"
    );
    // Байты материализации и их журнал остаются для переанализа.
    let batch = load(&f.store, "sel");
    assert!(batch.find_candidate(&produced.candidate_id).is_some());
}

/// Повреждённый blob материализации не выдаётся: SHA сверяется при чтении.
#[test]
fn tampered_materialized_blob_is_never_consumed() {
    let f = two_readings();
    select_by_user(&f.store, "なま", interval(&f.nama, 1), &f.source_sha);
    let report = materialize::materialize(&f.store, "sel", None).unwrap();
    let sha = report.materialized[0].sha256.clone();
    let blob = f
        .store
        .root()
        .join(".runtime/batches/sel/candidates")
        .join(format!("{sha}.mp3"));
    assert!(blob.exists(), "blob материализации: {}", blob.display());
    let mut permissions = std::fs::metadata(&blob).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(&blob, permissions).unwrap();
    std::fs::write(&blob, two_part_mp3()).unwrap();
    let requests = [Pronunciation::new("生", "なま").unwrap()];
    let error = read_selected(
        f.store.root(),
        "sel",
        &requests,
        ConsumerTrustPolicy::HumanDecision,
    )
    .unwrap_err();
    // Целость blob проверяет и runtime при загрузке, и чтение выбора.
    assert!(
        matches!(
            error.code,
            ErrorCode::IntegrityMismatch | ErrorCode::ManifestCorrupt
        ),
        "{error:?}"
    );
    let out = f._workspace.path().join("out");
    assert!(
        export_selected(
            f.store.root(),
            "sel",
            &requests,
            ConsumerTrustPolicy::HumanDecision,
            &out,
        )
        .is_err()
    );
    assert!(!out.join("生_なま.mp3").exists());
}

// ---------------------------------------------------------------------------
// Публикация без замены и без удаления чужого файла
// ---------------------------------------------------------------------------

fn leftovers(directory: &Path) -> Vec<String> {
    std::fs::read_dir(directory)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".tmp-export-"))
        .collect()
}

#[test]
fn foreign_file_created_before_publication_is_never_replaced() {
    let workspace = TempWorkspace::create("export-race").unwrap();
    let directory = workspace.path().to_owned();
    let ours = mp3(440);
    let sha = sha256_hex(&ours);
    let foreign = b"foreign bytes".to_vec();
    let target_bytes = foreign.clone();
    BEFORE_PUBLISH.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |target: &Path| {
            std::fs::write(target, &target_bytes).unwrap();
        }));
    });
    let error = persist_selected_file_for_test(&directory, "生_なま.mp3", &sha, &ours).unwrap_err();
    assert_eq!(error.code, ErrorCode::IdentityConflict);
    assert_eq!(
        std::fs::read(directory.join("生_なま.mp3")).unwrap(),
        foreign
    );
    assert!(leftovers(&directory).is_empty());

    // Конкурент записал те же байты: идемпотентный успех без замены.
    let same = ours.clone();
    BEFORE_PUBLISH.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |target: &Path| {
            std::fs::write(target, &same).unwrap();
        }));
    });
    let written = persist_selected_file_for_test(&directory, "生_せい.mp3", &sha, &ours).unwrap();
    assert!(!written);
    assert!(leftovers(&directory).is_empty());
}

#[test]
fn readback_mismatch_after_publication_never_deletes_the_path() {
    let workspace = TempWorkspace::create("export-readback").unwrap();
    let directory = workspace.path().to_owned();
    let ours = mp3(440);
    let sha = sha256_hex(&ours);
    // Чужой процесс заменил файл после публикации: отказ, но не удаление.
    AFTER_PUBLISH.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(|target: &Path| {
            std::fs::remove_file(target).unwrap();
            std::fs::write(target, b"replaced by another process").unwrap();
        }));
    });
    let error = persist_selected_file_for_test(&directory, "生_なま.mp3", &sha, &ours).unwrap_err();
    assert_eq!(error.code, ErrorCode::IntegrityMismatch);
    assert_eq!(error.details["code"], "export_readback_mismatch");
    assert_eq!(
        std::fs::read(directory.join("生_なま.mp3")).unwrap(),
        b"replaced by another process"
    );
    // Удаление перед обратной сверкой — ошибка ввода-вывода без побочных
    // действий с каталогом.
    AFTER_PUBLISH.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(|target: &Path| {
            std::fs::remove_file(target).unwrap();
        }));
    });
    let error = persist_selected_file_for_test(&directory, "生_せい.mp3", &sha, &ours).unwrap_err();
    assert_eq!(error.code, ErrorCode::IoFailure);
    assert!(!directory.join("生_せい.mp3").exists());
    assert!(leftovers(&directory).is_empty());
}

#[test]
fn no_replace_primitives_refuse_an_existing_target() {
    let workspace = TempWorkspace::create("export-primitives").unwrap();
    let target = workspace.path().join("生_なま.mp3");
    let temporary = workspace.path().join(".tmp-export-test");
    std::fs::write(&temporary, b"ours").unwrap();
    std::fs::write(&target, b"theirs").unwrap();
    let error = ops::publish_no_replace_for_test(&temporary, &target, b"ours").unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    let error = ops::create_new_with_for_test(&target, b"ours").unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&target).unwrap(), b"theirs");
    std::fs::remove_file(&target).unwrap();
    ops::create_new_with_for_test(&target, b"ours").unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"ours");
}

/// Параллельные экспорты: одинаковое содержимое — ровно одна запись и
/// идемпотентные повторы; разное — ровно один победитель, остальные
/// отказывают, файл победителя цел, временных файлов нет.
#[test]
fn concurrent_exports_never_replace_each_other() {
    let workspace = TempWorkspace::create("export-concurrent").unwrap();
    let directory = std::sync::Arc::new(workspace.path().to_owned());
    let same = std::sync::Arc::new(mp3(440));
    let sha = sha256_hex(same.as_slice());
    let results: Vec<_> = (0..8)
        .map(|_| {
            let (directory, same, sha) = (directory.clone(), same.clone(), sha.clone());
            std::thread::spawn(move || {
                persist_selected_file_for_test(&directory, "same.mp3", &sha, &same)
            })
        })
        .map(|t| t.join().unwrap())
        .collect();
    assert!(results.iter().all(Result::is_ok));
    assert_eq!(results.iter().filter(|r| *r.as_ref().unwrap()).count(), 1);
    assert_eq!(
        sha256_hex(std::fs::read(directory.join("same.mp3")).unwrap()),
        sha
    );

    let variants: Vec<Vec<u8>> = (0..6).map(|i| mp3(300 + 50 * i)).collect();
    let results: Vec<_> = variants
        .iter()
        .cloned()
        .map(|bytes| {
            let directory = directory.clone();
            std::thread::spawn(move || {
                let sha = sha256_hex(&bytes);
                (
                    sha.clone(),
                    persist_selected_file_for_test(&directory, "diff.mp3", &sha, &bytes),
                )
            })
        })
        .map(|t| t.join().unwrap())
        .collect();
    let winners: Vec<_> = results.iter().filter(|(_, r)| r.is_ok()).collect();
    assert_eq!(winners.len(), 1);
    for (_, result) in results.iter().filter(|(_, r)| r.is_err()) {
        assert_eq!(
            result.as_ref().unwrap_err().code,
            ErrorCode::IdentityConflict
        );
    }
    assert_eq!(
        sha256_hex(std::fs::read(directory.join("diff.mp3")).unwrap()),
        winners[0].0
    );
    assert!(leftovers(&directory).is_empty());
}

// ---------------------------------------------------------------------------
// Режим проверки пакета
// ---------------------------------------------------------------------------

fn mode(
    store: &AssetStore,
    mode: ReviewMode,
    agent: Option<&str>,
    trust: Option<ConsumerTrustPolicy>,
) -> Result<ops::ReviewModeReport, crate::AssetError> {
    set_review_mode(
        store,
        "sel",
        ReviewModeRequest {
            mode,
            agent: agent.map(str::to_owned),
            agent_trust: trust,
            channel: "agent-dialog".into(),
            statement: format!("режим {mode:?} {agent:?} {trust:?}"),
        },
    )
}

fn agent_decision(
    store: &AssetStore,
    agent: &str,
    link: &TargetLink,
) -> Result<(), crate::AssetError> {
    let candidate = &link.candidates[1];
    decide(
        store,
        "sel",
        DecideRequest {
            surface: "本".into(),
            reading: "ほん".into(),
            action: DecideAction::Select {
                candidate_id: candidate.candidate_id.clone(),
                expected_sha256: candidate.audio.as_ref().unwrap().sha256.clone(),
            },
            authority: DecisionAuthority::AgentEvidence {
                agent: agent.into(),
                policy: AGENT_EVIDENCE_POLICY.into(),
            },
            reason: "свидетельства ASR".into(),
            expected_fingerprint: None,
        },
    )
    .map(|_| ())
}

#[test]
fn review_mode_scopes_agent_authority_at_decision_and_consumption() {
    let (_workspace, store) = store();
    let intake = intake_files(
        &store,
        "sel",
        vec![IntakeFile {
            source_name: "本.mp3".into(),
            bytes: mp3(440),
        }],
        Some("本"),
    )
    .unwrap();
    let link = add_recognized_link(
        &store,
        "sel",
        &intake.added[0],
        "本",
        "ほん",
        "ほん",
        &mp3(550),
    );
    let requests = [Pronunciation::new("本", "ほん").unwrap()];
    let read = |policy| read_selected_info(store.root(), "sel", &requests, policy);

    // Делегирование требует агента и границы доверия.
    assert!(mode(&store, ReviewMode::DelegatedAgent, None, None).is_err());
    assert!(
        mode(
            &store,
            ReviewMode::DelegatedAgent,
            Some("codex"),
            Some(ConsumerTrustPolicy::HumanDecision)
        )
        .is_err()
    );

    // Ручной режим: агент не решает, бесспорное решает автоматика.
    mode(&store, ReviewMode::ManualUser, None, None).unwrap();
    let refused = agent_decision(&store, "codex", &link).unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidValidationEvidence);
    assert!(
        auto_select(
            &store,
            "sel",
            Some(&AgentPolicy {
                agent: "codex".into()
            })
        )
        .is_err()
    );
    let report = auto_select(&store, "sel", None).unwrap();
    assert_eq!(report.selected_deterministic, 1);
    assert_eq!(report.review_mode, Some(ReviewMode::ManualUser));
    assert!(read(ConsumerTrustPolicy::AgentSelected).is_ok());

    // Делегирование только через аудиоэкспертизу: детерминированное решение
    // перестаёт действовать без переписывания журнала.
    let changed = mode(
        &store,
        ReviewMode::DelegatedAgent,
        Some("codex"),
        Some(ConsumerTrustPolicy::HumanOrAudioModel),
    )
    .unwrap();
    assert_eq!(changed.agent_decisions_suspended, 1);
    assert!(read(ConsumerTrustPolicy::AgentSelected).is_err());
    assert!(agent_decision(&store, "codex", &link).is_err());
    let status = ops::status_value(&store, "sel", GroupFilter::Unresolved, 50).unwrap();
    assert_eq!(status["review_mode"]["mode"], "delegated_agent");
    assert_eq!(status["matched"], 1);

    // Расширенное делегирование: агент решает сам, но остаётся агентом.
    mode(
        &store,
        ReviewMode::DelegatedAgent,
        Some("codex"),
        Some(ConsumerTrustPolicy::AgentSelected),
    )
    .unwrap();
    assert!(read(ConsumerTrustPolicy::AgentSelected).is_ok());
    assert!(agent_decision(&store, "other-agent", &link).is_err());
    agent_decision(&store, "codex", &link).unwrap();
    let info = read(ConsumerTrustPolicy::AgentSelected).unwrap();
    assert_eq!(info[0].authority.kind(), "agent_evidence");
    assert!(!info[0].authority.human_listened());
    assert!(read(ConsumerTrustPolicy::HumanDecision).is_err());
    assert!(
        auto_select(
            &store,
            "sel",
            Some(&AgentPolicy {
                agent: "other-agent".into()
            })
        )
        .is_err()
    );

    // Возврат к ручному режиму приостанавливает решение агента.
    let manual = mode(&store, ReviewMode::ManualUser, None, None).unwrap();
    assert_eq!(manual.agent_decisions_suspended, 1);
    assert!(read(ConsumerTrustPolicy::AgentSelected).is_err());
    let batch = load(&store, "sel");
    assert_eq!(batch.review_modes.len(), 4);
    assert!(
        batch
            .review_modes
            .iter()
            .all(|g| g.statement_sha256.len() == 64)
    );
}

/// Решение пользователя по интервалу из ручного обзора применяется из файла,
/// сохранённого страницей, и проверяется по снимку.
#[test]
fn review_page_decisions_file_is_applied_only_through_the_snapshot_contract() {
    let f = two_readings();
    mode(&f.store, ReviewMode::ManualUser, None, None).unwrap();
    let exported = review::export(&f.store, "sel", GroupFilter::Unresolved, 0, 50).unwrap();
    assert_eq!(exported.user_instructions.len(), 3);
    let directory = Path::new(&exported.directory);
    let html = std::fs::read_to_string(directory.join("index.html")).unwrap();
    assert!(html.contains("save-decisions"));
    assert!(html.contains(&format!("decisions-{}.tsv", exported.review_id)));
    let rows = snapshot_rows(directory);
    let candidate = interval(&f.nama, 1);
    let number = option_number(&rows, "なま", &candidate.candidate_id);
    // Файл решений «из браузера» — копия шаблона с заполненной колонкой.
    let saved = f._workspace.path().join("decisions-saved.tsv");
    std::fs::copy(directory.join("decisions.tsv"), &saved).unwrap();
    set_action(&saved, &rows, "なま", &format!("heard:{number}"));
    let text = std::fs::read_to_string(&saved).unwrap();
    // Подмена неизменяемой колонки отвергается целиком.
    let tampered = text.replacen("occurrence_interval", "source_original", 1);
    assert!(review::apply_with(&f.store, "sel", &exported.review_id, Some(&tampered)).is_err());
    let applied = review::apply_with(&f.store, "sel", &exported.review_id, Some(&text)).unwrap();
    assert_eq!(applied.selected, 1);
    // Повтор того же файла идемпотентен; применённое — решение пользователя.
    let again = review::apply_with(&f.store, "sel", &exported.review_id, Some(&text)).unwrap();
    assert!(!again.changed);
    let batch = load(&f.store, "sel");
    let decision = batch.current_decision("生", "なま").unwrap();
    assert!(matches!(
        decision.authority,
        DecisionAuthority::UserDirectReview {
            user_listened: true,
            ..
        }
    ));
    // Пользователь выбрал интервал; MP3 появляется после материализации.
    let batch_index = engine::SelectionIndex::new(&batch);
    assert_eq!(
        ops::selection_readiness(&batch_index, &batch, decision),
        Some(Readiness::PendingMaterialization)
    );
    materialize::materialize(&f.store, "sel", Some(("生", "なま"))).unwrap();
    let requests = [Pronunciation::new("生", "なま").unwrap()];
    assert!(
        read_selected(
            f.store.root(),
            "sel",
            &requests,
            ConsumerTrustPolicy::HumanDecision
        )
        .is_ok()
    );
}

/// Старое состояние с `plan_reading` строкой читается как подсказка, а
/// Forvo-исходник полного сбора не имеет чтения вовсе.
#[test]
fn forvo_origin_reading_is_an_optional_hint() {
    let sha = sha(7);
    let legacy = serde_json::json!({
        "kind": "forvo_batch",
        "batch_id": "forvo",
        "surface": "生",
        "evidence": forvo_evidence("生", &sha, 1),
        "plan_reading": "なま",
    });
    let origin: SourceOrigin = serde_json::from_value(legacy).unwrap();
    assert!(matches!(
        &origin,
        SourceOrigin::ForvoBatch { plan_reading: Some(r), .. } if r == "なま"
    ));
    let surface_only = SourceOrigin::ForvoBatch {
        batch_id: "forvo".into(),
        surface: Some("生".into()),
        evidence: forvo_evidence("生", &sha, 1),
        plan_reading: None,
    };
    let encoded = serde_json::to_value(&surface_only).unwrap();
    assert!(encoded.get("plan_reading").is_none());
    let mut source = fake_source(7, "生", true);
    source.origins = vec![surface_only];
    batch_with(vec![source], Vec::new()).validate().unwrap();
}
