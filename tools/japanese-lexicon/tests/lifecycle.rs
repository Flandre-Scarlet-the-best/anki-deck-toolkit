//! Жизненный цикл словарей на синтетических архивах и поддельном источнике публикации.

mod common;

use common::*;
use japanese_lexicon::catalog;
use japanese_lexicon::error::Code;
use japanese_lexicon::lookup::{self, EvidenceClass, Lexicon, Status};
use japanese_lexicon::manager::{self, Options, Outcome, State};
use japanese_lexicon::store::{self, Location, Origin, Store};
use japanese_lexicon::yomitan::Limits;
use serde_json::json;
use std::path::Path;
use std::sync::atomic::Ordering;

fn all() -> Vec<&'static catalog::Source> {
    catalog::standard().iter().collect()
}

fn one(id: &str) -> Vec<&'static catalog::Source> {
    vec![catalog::find(id).unwrap()]
}

fn location(root: &Path) -> Location {
    Location {
        root: root.to_path_buf(),
        origin: Origin::Option,
    }
}

fn outcome(report: &manager::OperationReport, id: &str) -> Outcome {
    report.items.iter().find(|i| i.id == id).unwrap().outcome
}

fn code(report: &manager::OperationReport, id: &str) -> Option<Code> {
    report.items.iter().find(|i| i.id == id).unwrap().code
}

fn lookup(root: &Path, surface: &str) -> lookup::LookupResult {
    Lexicon::open(root)
        .unwrap()
        .lookup(surface, &lookup::Options::default())
        .unwrap()
}

fn installed(root: &Path) -> (Location, FakeUpstream) {
    let up = FakeUpstream::standard();
    let report = manager::install(root, &all(), &up, None, &Options::default()).unwrap();
    assert!(report.ok, "{report:#?}");
    for s in catalog::standard() {
        assert_eq!(outcome(&report, s.id), Outcome::Installed);
    }
    (location(root), up)
}

#[test]
fn install_status_verify_and_lookup_distinguish_every_class() {
    let tmp = TempDir::new("cycle");
    let root = tmp.path().join("data");
    let (loc, up) = installed(&root);

    let status = manager::status(&loc).unwrap();
    assert!(status.dictionaries.iter().all(|d| d.state == State::Ok));
    let jpdb = status
        .dictionaries
        .iter()
        .find(|d| d.id == "jpdb-v2.2-freq-kana")
        .unwrap();
    // Из листинга выбран файл Kana с самой поздней датой, а не Display Only.
    assert_eq!(
        jpdb.installed.as_ref().unwrap().release.as_deref(),
        Some("2024-10-13")
    );
    assert!(jpdb.installed.as_ref().unwrap().published_checksum_verified);
    assert!(manager::verify(&loc, &all()).unwrap().ok);

    // Обычное слово с одним чтением.
    let r = lookup(&root, "学校");
    assert_eq!(r.status, Status::SingleReading);
    assert_eq!(r.distinct_readings, vec!["がっこう"]);
    assert!(r.complete);
    let g = &r.readings[0];
    assert_eq!(g.class, EvidenceClass::LexicalEntry);
    assert_eq!(g.evidence[0].dictionary.as_ref().unwrap().id, "jitendex");
    assert_eq!(g.evidence[0].entry.as_ref().unwrap().sequence, Some(1));
    assert_eq!(g.frequency[0].rank, Some(616));
    // Частотность чтения, которого нет в словарях, не стала чтением.
    assert!(r.readings.iter().all(|g| g.normalized != "まなびや"));
    assert_eq!(r.frequency_unmatched.len(), 1);
    let targets = r.candidate_targets();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].surface, "学校");
    assert_eq!(targets[0].reading, "がっこう");
    assert_eq!(targets[0].classes, vec![EvidenceClass::LexicalEntry]);
    assert_eq!(targets[0].dictionaries, vec!["jitendex"]);

    // Неоднозначное слово: все чтения с происхождением; имя собственное отдельно.
    let r = lookup(&root, "今日");
    assert_eq!(r.status, Status::MultipleReadings);
    assert_eq!(r.distinct_readings, vec!["きょう", "こんにち"]);
    let names: Vec<_> = r
        .readings
        .iter()
        .filter(|g| g.class == EvidenceClass::ProperName)
        .collect();
    assert_eq!(names.len(), 1);
    assert_eq!(
        names[0].evidence[0].dictionary.as_ref().unwrap().id,
        "jmnedict"
    );
    let kyou = &r.readings[0];
    assert_eq!(kyou.normalized, "きょう");
    let tag_names: Vec<_> = kyou.evidence[0]
        .tags
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    assert_eq!(tag_names, vec!["★", "priority\u{a0}form"]);

    // Одиночный кандзи: лексика отдельно от чтений символа.
    let r = lookup(&root, "生");
    assert_eq!(r.status, Status::MultipleReadings);
    assert_eq!(r.distinct_readings, vec!["せい", "なま"]);
    let kanji: Vec<_> = r
        .readings
        .iter()
        .filter(|g| g.class == EvidenceClass::KanjiReference)
        .collect();
    let forms: Vec<_> = kanji.iter().map(|g| g.normalized.as_str()).collect();
    assert_eq!(forms, vec!["せい", "しょう", "いきる", "なま", "う"]);
    let iki = kanji.iter().find(|g| g.normalized == "いきる").unwrap();
    assert_eq!(iki.evidence[0].character_reading.as_deref(), Some("い"));
    assert_eq!(iki.evidence[0].okurigana.as_deref(), Some("きる"));
    let affix = |form: &str| {
        kanji
            .iter()
            .find(|g| g.normalized == form)
            .unwrap()
            .evidence[0]
            .affix
    };
    assert_eq!(affix("なま"), Some("prefix"));
    assert_eq!(affix("う"), Some("suffix"));
    assert_eq!(affix("いきる"), None);
    assert!(
        r.candidate_targets()
            .iter()
            .all(|c| !c.classes.contains(&EvidenceClass::KanjiReference))
    );

    // Кандзи без лексической статьи — только ограниченный статус.
    let r = lookup(&root, "学");
    assert_eq!(r.status, Status::KanjiReferenceOnly);
    assert!(r.distinct_readings.is_empty());

    // Хирагана и катакана.
    let r = lookup(&root, "がっこう");
    assert_eq!(r.status, Status::SingleReading);
    assert_eq!(r.readings[0].class, EvidenceClass::SurfaceKana);
    assert_eq!(r.readings[0].frequency[0].display.as_deref(), Some("5㋕"));
    assert!(r.readings[0].frequency[0].kana_writing);
    let r = lookup(&root, "カメラ");
    assert_eq!(r.status, Status::SingleReading);
    assert_eq!(r.distinct_readings, vec!["かめら"]);
    assert!(
        r.readings
            .iter()
            .any(|g| g.class == EvidenceClass::LexicalEntry)
    );
    assert!(
        r.readings
            .iter()
            .any(|g| g.class == EvidenceClass::SurfaceKana && g.reading == "カメラ")
    );
    assert_eq!(lookup(&root, "ｶﾒﾗ").status, Status::Ambiguous);

    // Имя собственное не становится лексическим чтением.
    let r = lookup(&root, "田中");
    assert_eq!(r.status, Status::ProperNameOnly);
    assert!(r.distinct_readings.is_empty());
    let r = lookup(&root, "東京");
    assert_eq!(r.status, Status::ProperNameOnly);
    assert_eq!(r.readings[0].reading, "トンキン");

    // Указатель на другую форму — не чтение.
    let r = lookup(&root, "勞働");
    assert_eq!(r.status, Status::RedirectOnly);
    assert_eq!(r.redirects[0].target, "労働");
    assert_eq!(r.redirects[0].target_reading.as_deref(), Some("ろうどう"));
    assert!(r.readings.is_empty());

    assert_eq!(lookup(&root, "謎字").status, Status::NotFound);
    assert_eq!(lookup(&root, "存在しない").status, Status::NotFound);

    // NFC: разложенная が находит то же, что составная.
    let r = lookup(&root, "\u{304B}\u{3099}っこう");
    assert!(r.query.nfc_changed);
    assert_eq!(r.query.surface, "がっこう");

    // Повторная установка ничего не скачивает и не трогает сеть.
    let before = up.requests.load(Ordering::SeqCst);
    let again = manager::install(&root, &all(), &up, None, &Options::default()).unwrap();
    assert!(
        again
            .items
            .iter()
            .all(|i| i.outcome == Outcome::AlreadyInstalled)
    );
    assert_eq!(up.requests.load(Ordering::SeqCst), before);
}

#[test]
fn check_and_update_do_not_touch_unchanged_data() {
    let tmp = TempDir::new("noop");
    let root = tmp.path().join("data");
    let (_, up) = installed(&root);
    let before = snapshot(&root);
    let downloads = up.downloads.load(Ordering::SeqCst);

    let check = manager::check_updates(&root, &all(), &up).unwrap();
    assert!(
        check.items.iter().all(|i| i.outcome == Outcome::UpToDate),
        "{check:#?}"
    );
    let update = manager::update(&root, &all(), &up, &Options::default()).unwrap();
    assert!(
        update.items.iter().all(|i| i.outcome == Outcome::UpToDate),
        "{update:#?}"
    );
    assert_eq!(up.downloads.load(Ordering::SeqCst), downloads);
    assert_eq!(snapshot(&root), before);
}

#[test]
fn update_replaces_only_the_changed_source() {
    let tmp = TempDir::new("update");
    let root = tmp.path().join("data");
    let (loc, up) = installed(&root);
    let extra = json!(["新語", "しんご", "", "", 0, [{"type": "structured-content", "content": "new word"}], 99, ""]);
    up.publish("jitendex", "T2", jitendex("T2", &[extra]));

    let before = snapshot(&root);
    let check = manager::check_updates(&root, &all(), &up).unwrap();
    assert_eq!(outcome(&check, "jitendex"), Outcome::UpdateAvailable);
    for id in ["jmnedict", "kanjidic-english", "jpdb-v2.2-freq-kana"] {
        assert_eq!(outcome(&check, id), Outcome::UpToDate);
    }
    assert_eq!(snapshot(&root), before, "check-updates изменил данные");
    assert_eq!(lookup(&root, "新語").status, Status::NotFound);

    let old_version = Store::open_existing(&root)
        .unwrap()
        .unwrap()
        .registry()
        .unwrap()
        .dictionaries["jitendex"]
        .version
        .clone();
    let update = manager::update(&root, &all(), &up, &Options::default()).unwrap();
    assert!(update.ok);
    assert_eq!(outcome(&update, "jitendex"), Outcome::Updated);
    let item = update.items.iter().find(|i| i.id == "jitendex").unwrap();
    assert_eq!(item.previous.as_ref().unwrap().revision, "T1");
    assert_eq!(item.installed.as_ref().unwrap().revision, "T2");
    assert_eq!(outcome(&update, "jmnedict"), Outcome::UpToDate);
    assert_eq!(lookup(&root, "新語").distinct_readings, vec!["しんご"]);
    assert!(
        !root
            .join("dictionaries/jitendex")
            .join(old_version)
            .exists()
    );
    assert!(manager::verify(&loc, &all()).unwrap().ok);

    // Повтор безопасен.
    let again = manager::update(&root, &all(), &up, &Options::default()).unwrap();
    assert!(again.items.iter().all(|i| i.outcome == Outcome::UpToDate));
}

#[test]
fn partial_failure_reports_each_source_and_keeps_old_data() {
    let tmp = TempDir::new("partial");
    let root = tmp.path().join("data");
    let (_, up) = installed(&root);
    up.publish("jitendex", "T2", jitendex("T2", &[]));
    up.publish("jmnedict", "2026-02-02", jmnedict("JMnedict.2026-02-02"));
    up.fail(
        "https://github.com/yomidevs/jmdict-yomitan/releases/download/2026-02-02/JMnedict.zip",
        Code::UpstreamUnavailable,
    );
    let update = manager::update(&root, &all(), &up, &Options::default()).unwrap();
    assert!(!update.ok);
    assert_eq!(outcome(&update, "jitendex"), Outcome::Updated);
    assert_eq!(outcome(&update, "jmnedict"), Outcome::Failed);
    assert_eq!(code(&update, "jmnedict"), Some(Code::UpstreamUnavailable));
    // kanjidic вышел в том же релизе, его содержимое не изменилось.
    assert_eq!(outcome(&update, "kanjidic-english"), Outcome::UpToDate);
    let r = lookup(&root, "田中");
    assert_eq!(r.status, Status::ProperNameOnly);
    assert_eq!(
        r.readings[0].evidence[0]
            .dictionary
            .as_ref()
            .unwrap()
            .revision,
        "JMnedict.2026-01-01"
    );
    // После отказа ложного up_to_date нет.
    let check = manager::check_updates(&root, &one("jmnedict"), &up).unwrap();
    assert_eq!(outcome(&check, "jmnedict"), Outcome::UpdateAvailable);
}

/// Каждый сбой обновления оставляет прежнюю активную версию и не даёт
/// ложного `up_to_date`.
#[test]
fn failed_updates_keep_the_last_good_version() {
    let tmp = TempDir::new("faults");
    let root = tmp.path().join("data");
    let (loc, up) = installed(&root);
    let good = snapshot(&root.join("dictionaries"));

    let unsafe_zip = |entries: Vec<Entry>| {
        let mut all = vec![
            file(
                "index.json",
                &json!({"title": "Jitendex.org [T9]", "format": 3, "revision": "T9"}),
            ),
            file(
                "term_bank_1.json",
                &json!([["学校", "がっこう", "", "", 0, [], 1, ""]]),
            ),
        ];
        all.extend(entries);
        zip_bytes(all)
    };
    let cases: Vec<(&str, Vec<u8>, Code)> = vec![
        (
            "corrupt",
            b"PK\x03\x04 definitely not a zip".to_vec(),
            Code::InvalidFormat,
        ),
        (
            "schema",
            zip_bytes(vec![
                file(
                    "index.json",
                    &json!({"title": "Jitendex.org [T9]", "format": 2, "revision": "T9"}),
                ),
                file("term_bank_1.json", &json!([])),
            ]),
            Code::UnsupportedSchema,
        ),
        (
            "traversal",
            unsafe_zip(vec![Entry::File("../evil.css".into(), b"x".to_vec())]),
            Code::UnsafeArchive,
        ),
        (
            "symlink",
            unsafe_zip(vec![Entry::Symlink(
                "link.css".into(),
                "/etc/passwd".into(),
            )]),
            Code::UnsafeArchive,
        ),
        (
            "unexpected",
            unsafe_zip(vec![Entry::File("run.sh".into(), b"x".to_vec())]),
            Code::InvalidFormat,
        ),
        (
            "bank",
            unsafe_zip(vec![Entry::File(
                "kanji_bank_1.json".into(),
                b"[]".to_vec(),
            )]),
            Code::UnsupportedSchema,
        ),
        (
            // Оборванное индексирование: первый банк корректен, второй — нет.
            "midway",
            unsafe_zip(vec![Entry::File(
                "term_bank_2.json".into(),
                b"[[\"x\", ".to_vec(),
            )]),
            Code::InvalidFormat,
        ),
        (
            "revision",
            zip_bytes(vec![
                file(
                    "index.json",
                    &json!({"title": "Jitendex.org [X]", "format": 3, "revision": "other"}),
                ),
                file("term_bank_1.json", &json!([])),
            ]),
            Code::UpstreamUnexpected,
        ),
        (
            "foreign",
            zip_bytes(vec![
                file(
                    "index.json",
                    &json!({"title": "Some other dictionary", "format": 3, "revision": "T9"}),
                ),
                file("term_bank_1.json", &json!([])),
            ]),
            Code::InvalidFormat,
        ),
    ];
    for (name, bytes, expected) in cases {
        up.publish("jitendex", "T9", bytes);
        let update = manager::update(&root, &one("jitendex"), &up, &Options::default()).unwrap();
        assert_eq!(
            outcome(&update, "jitendex"),
            Outcome::Failed,
            "{name}: {update:#?}"
        );
        assert_eq!(
            code(&update, "jitendex"),
            Some(expected),
            "{name}: {update:#?}"
        );
        assert_eq!(snapshot(&root.join("dictionaries")), good, "{name}");
        assert!(
            std::fs::read_dir(root.join("staging"))
                .unwrap()
                .next()
                .is_none(),
            "{name}: staging"
        );
        assert_eq!(
            lookup(&root, "学校").status,
            Status::SingleReading,
            "{name}"
        );
        let check = manager::check_updates(&root, &one("jitendex"), &up).unwrap();
        assert_eq!(
            outcome(&check, "jitendex"),
            Outcome::UpdateAvailable,
            "{name}"
        );
    }

    // Опубликованная сумма не совпала с полученными байтами.
    up.publish("jitendex", "T9", jitendex("T9", &[]));
    up.tamper_digest("jitendex", &"ab".repeat(32));
    let update = manager::update(&root, &one("jitendex"), &up, &Options::default()).unwrap();
    assert_eq!(code(&update, "jitendex"), Some(Code::ChecksumMismatch));
    assert_eq!(snapshot(&root.join("dictionaries")), good);

    // 404 и недоступная сеть при обнаружении.
    up.fail(JITENDEX_API, Code::UpstreamUnexpected);
    let update = manager::update(&root, &one("jitendex"), &up, &Options::default()).unwrap();
    assert_eq!(code(&update, "jitendex"), Some(Code::UpstreamUnexpected));
    up.fail(JITENDEX_API, Code::UpstreamUnavailable);
    let check = manager::check_updates(&root, &one("jitendex"), &up).unwrap();
    assert_eq!(outcome(&check, "jitendex"), Outcome::CheckFailed);
    assert_eq!(code(&check, "jitendex"), Some(Code::UpstreamUnavailable));
    up.fail(JITENDEX_API, Code::UpstreamRateLimited);
    let check = manager::check_updates(&root, &one("jitendex"), &up).unwrap();
    assert_eq!(code(&check, "jitendex"), Some(Code::UpstreamRateLimited));
    assert!(manager::verify(&loc, &all()).unwrap().ok);
}

#[test]
fn archive_limits_reject_bombs_and_oversized_archives() {
    let tmp = TempDir::new("limits");
    let root = tmp.path().join("data");
    let up = FakeUpstream::standard();
    let mut bomb = vec![
        file(
            "index.json",
            &json!({"title": "Jitendex.org [T1]", "format": 3, "revision": "T1"}),
        ),
        file("term_bank_1.json", &json!([])),
    ];
    bomb.push(Entry::File("zeros.css".into(), vec![0u8; 4 * 1024 * 1024]));
    up.publish("jitendex", "T1", zip_bytes(bomb));
    let strict = Options {
        limits: Limits {
            max_ratio: 50,
            ..Limits::default()
        },
    };
    let report = manager::install(&root, &one("jitendex"), &up, None, &strict).unwrap();
    assert_eq!(code(&report, "jitendex"), Some(Code::UnsafeArchive));

    up.publish("jitendex", "T1", jitendex("T1", &[]));
    let few = Options {
        limits: Limits {
            max_entries: 2,
            ..Limits::default()
        },
    };
    let report = manager::install(&root, &one("jitendex"), &up, None, &few).unwrap();
    assert_eq!(code(&report, "jitendex"), Some(Code::LimitExceeded));
    assert!(
        Store::open_existing(&root)
            .unwrap()
            .unwrap()
            .registry()
            .unwrap()
            .dictionaries
            .is_empty()
    );
}

#[test]
fn remove_one_keeps_the_rest_and_reinstall_restores_it() {
    let tmp = TempDir::new("remove");
    let root = tmp.path().join("data");
    let (loc, up) = installed(&root);
    std::fs::write(root.join("notes.txt"), b"user file").unwrap();

    let report = manager::remove(&root, &one("jmnedict")).unwrap();
    assert_eq!(outcome(&report, "jmnedict"), Outcome::Removed);
    assert!(!root.join("dictionaries/jmnedict").exists());
    let r = lookup(&root, "田中");
    assert_eq!(r.status, Status::NotFound);
    assert!(!r.complete);
    assert_eq!(r.missing_sources, vec!["jmnedict"]);
    assert_eq!(lookup(&root, "学校").status, Status::SingleReading);
    assert_eq!(
        manager::remove(&root, &one("jmnedict")).unwrap().items[0].outcome,
        Outcome::NotInstalled
    );

    let report = manager::install(&root, &one("jmnedict"), &up, None, &Options::default()).unwrap();
    assert_eq!(outcome(&report, "jmnedict"), Outcome::Installed);
    assert_eq!(lookup(&root, "田中").status, Status::ProperNameOnly);

    // Без лексики кандзи честно сообщает об отсутствии источника, а кана — нет.
    manager::remove(&root, &one("jitendex")).unwrap();
    let r = lookup(&root, "学校");
    assert_eq!(r.status, Status::SourceMissing);
    assert!(r.missing_sources.contains(&"jitendex"));
    let r = lookup(&root, "がっこう");
    assert_eq!(r.status, Status::SingleReading);
    assert_eq!(r.readings[0].class, EvidenceClass::SurfaceKana);
    // Сомнительная кана остаётся сомнительной: Jitendex её не исправит.
    let r = lookup(&root, "ｶﾒﾗ");
    assert_eq!(r.status, Status::Ambiguous);
    assert!(!r.kana.as_ref().unwrap().issues.is_empty());
    let r = lookup(&root, "生");
    assert_eq!(r.status, Status::SourceMissing);
    assert!(
        r.readings
            .iter()
            .any(|g| g.class == EvidenceClass::KanjiReference)
    );

    let report = manager::remove(&root, &all()).unwrap();
    assert!(report.ok);
    assert_eq!(report.untouched, vec![root.join("notes.txt")]);
    assert_eq!(std::fs::read(root.join("notes.txt")).unwrap(), b"user file");
    assert!(
        Store::open_existing(&root)
            .unwrap()
            .unwrap()
            .registry()
            .unwrap()
            .dictionaries
            .is_empty()
    );
    for s in catalog::standard() {
        assert!(!root.join("dictionaries").join(s.id).exists());
    }
    let status = manager::status(&loc).unwrap();
    assert!(
        status
            .dictionaries
            .iter()
            .all(|d| d.state == State::NotInstalled)
    );
    assert_eq!(lookup(&root, "学校").status, Status::SourceMissing);
}

#[test]
fn concurrent_mutation_is_refused_and_open_lookup_survives_replacement() {
    let tmp = TempDir::new("concurrency");
    let root = tmp.path().join("data");
    let (_, up) = installed(&root);
    let store = Store::open_existing(&root).unwrap().unwrap();
    {
        let _held = store.lock_operations().unwrap();
        assert!(
            manager::status(&location(&root))
                .unwrap()
                .operation_in_progress
        );
        let err = manager::update(&root, &all(), &up, &Options::default()).unwrap_err();
        assert_eq!(err.code, Code::Busy);
        let err = manager::remove(&root, &all()).unwrap_err();
        assert_eq!(err.code, Code::Busy);
        // Поиск не ждёт изменяющую операцию.
        assert_eq!(lookup(&root, "学校").status, Status::SingleReading);
    }
    // Проба status/verify не создаёт файл блокировки и не мешает операциям.
    std::fs::remove_file(root.join(store::OPERATIONS_LOCK)).unwrap();
    let status = manager::status(&location(&root)).unwrap();
    assert!(!status.operation_in_progress);
    assert!(!root.join(store::OPERATIONS_LOCK).exists());

    // Открытый поиск продолжает работать после замены и удаления версии.
    let lexicon = Lexicon::open(&root).unwrap();
    up.publish("jitendex", "T2", jitendex("T2", &[]));
    assert_eq!(
        outcome(
            &manager::update(&root, &one("jitendex"), &up, &Options::default()).unwrap(),
            "jitendex"
        ),
        Outcome::Updated
    );
    manager::remove(&root, &all()).unwrap();
    let r = lexicon.lookup("学校", &lookup::Options::default()).unwrap();
    assert_eq!(r.status, Status::SingleReading);
    assert_eq!(
        r.readings[0].evidence[0]
            .dictionary
            .as_ref()
            .unwrap()
            .revision,
        "T1"
    );

    // Параллельные установки в потоках: ровно одна выполняет работу.
    let tmp2 = TempDir::new("race");
    let root2 = tmp2.path().join("data");
    let up2 = FakeUpstream::standard();
    let results: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(|| manager::install(&root2, &all(), &up2, None, &Options::default()))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut busy = 0;
    for r in &results {
        match r {
            Ok(report) => assert!(report.ok),
            Err(e) => {
                assert_eq!(e.code, Code::Busy);
                busy += 1;
            }
        }
    }
    assert!(busy < results.len());
    assert!(manager::verify(&location(&root2), &all()).unwrap().ok);
}

#[test]
fn recovery_after_interrupted_operation_and_repair_of_corruption() {
    let tmp = TempDir::new("recovery");
    let root = tmp.path().join("data");
    let (loc, up) = installed(&root);
    // Остановка посреди операции: незавершённый staging и неактивированная версия.
    std::fs::create_dir_all(root.join("staging/deadbeef")).unwrap();
    std::fs::write(root.join("staging/deadbeef/archive.zip"), b"partial").unwrap();
    std::fs::create_dir_all(root.join("dictionaries/jitendex/0000000000000000-00000000")).unwrap();

    let verify = manager::verify(&loc, &all()).unwrap();
    assert!(verify.ok, "остатки — не повреждение активных данных");
    assert_eq!(verify.leftovers.len(), 2);
    assert!(
        root.join("staging/deadbeef").exists(),
        "verify ничего не удаляет"
    );

    let update = manager::update(&root, &all(), &up, &Options::default()).unwrap();
    assert_eq!(update.cleaned.len(), 2);
    assert!(!root.join("staging/deadbeef").exists());

    // Повреждённый индекс: verify находит, lookup и update восстанавливают.
    let version = Store::open_existing(&root)
        .unwrap()
        .unwrap()
        .registry()
        .unwrap()
        .dictionaries["jitendex"]
        .version
        .clone();
    let index = root
        .join("dictionaries/jitendex")
        .join(&version)
        .join(store::INDEX_FILE);
    let mut bytes = std::fs::read(&index).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xFF;
    std::fs::write(&index, &bytes).unwrap();
    let verify = manager::verify(&loc, &all()).unwrap();
    assert!(!verify.ok);
    let jit = verify
        .dictionaries
        .iter()
        .find(|d| d.id == "jitendex")
        .unwrap();
    assert_eq!(jit.state, State::Corrupt);
    assert_eq!(jit.problems[0].code, Code::IndexCorrupt);
    let check = manager::check_updates(&root, &one("jitendex"), &up).unwrap();
    assert_eq!(
        outcome(&check, "jitendex"),
        Outcome::UpToDate,
        "сумма архива та же"
    );
    let update = manager::update(&root, &one("jitendex"), &up, &Options::default()).unwrap();
    assert_eq!(
        outcome(&update, "jitendex"),
        Outcome::Updated,
        "{update:#?}"
    );
    assert!(manager::verify(&loc, &all()).unwrap().ok);

    // Испорченный реестр — повреждение, а не «не установлено».
    std::fs::write(root.join(store::REGISTRY_FILE), b"{").unwrap();
    assert_eq!(Lexicon::open(&root).err().unwrap().code, Code::IndexCorrupt);
}

#[test]
fn foreign_and_repository_directories_are_refused() {
    let tmp = TempDir::new("owner");
    let up = FakeUpstream::standard();
    let foreign = tmp.path().join("foreign");
    std::fs::create_dir_all(&foreign).unwrap();
    std::fs::write(foreign.join("photo.jpg"), b"x").unwrap();
    let err = manager::install(&foreign, &all(), &up, None, &Options::default()).unwrap_err();
    assert_eq!(err.code, Code::NotOwned);
    assert_eq!(
        manager::remove(&foreign, &all()).unwrap_err().code,
        Code::NotOwned
    );
    assert_eq!(Lexicon::open(&foreign).err().unwrap().code, Code::NotOwned);
    assert_eq!(std::fs::read_dir(&foreign).unwrap().count(), 1);

    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let err = manager::install(
        &repo.join("lexicon"),
        &all(),
        &up,
        None,
        &Options::default(),
    )
    .unwrap_err();
    assert_eq!(err.code, Code::NotOwned);
    assert!(!repo.join("lexicon").exists());
}

#[test]
fn local_zip_import_works_without_network() {
    let tmp = TempDir::new("local");
    let root = tmp.path().join("data");
    let up = FakeUpstream::standard();
    up.fail(JMDICT_API, Code::UpstreamUnavailable);
    let report = manager::install(&root, &one("jmnedict"), &up, None, &Options::default()).unwrap();
    assert_eq!(code(&report, "jmnedict"), Some(Code::UpstreamUnavailable));
    let zip = tmp.path().join("JMnedict.zip");
    std::fs::write(&zip, jmnedict("JMnedict.2026-01-01")).unwrap();
    let report = manager::install(
        &root,
        &one("jmnedict"),
        &up,
        Some(&zip),
        &Options::default(),
    )
    .unwrap();
    assert_eq!(outcome(&report, "jmnedict"), Outcome::Installed);
    let installed = report.items[0].installed.as_ref().unwrap();
    assert_eq!(installed.provider, "local_file");
    assert!(!installed.published_checksum_verified);
    assert_eq!(lookup(&root, "田中").status, Status::SourceMissing);
    // Тот же архив опубликован upstream: проверка распознаёт его по сумме.
    up.heal();
    let check = manager::check_updates(&root, &one("jmnedict"), &up).unwrap();
    assert_eq!(outcome(&check, "jmnedict"), Outcome::UpToDate);
    // Чужой архив под чужим ID отвергается.
    let wrong = tmp.path().join("kanji.zip");
    std::fs::write(&wrong, kanjidic("x")).unwrap();
    let report = manager::install(
        &root,
        &one("jitendex"),
        &up,
        Some(&wrong),
        &Options::default(),
    )
    .unwrap();
    assert_eq!(code(&report, "jitendex"), Some(Code::UnsupportedSchema));
}

#[test]
fn lookup_output_is_deterministic_and_paginated() {
    let tmp = TempDir::new("pages");
    let root = tmp.path().join("data");
    installed(&root);
    let lexicon = Lexicon::open(&root).unwrap();
    let full = lexicon.lookup("生", &lookup::Options::default()).unwrap();
    let again = lexicon.lookup("生", &lookup::Options::default()).unwrap();
    assert_eq!(
        serde_json::to_string(&full).unwrap(),
        serde_json::to_string(&again).unwrap()
    );
    let page = lexicon
        .lookup(
            "生",
            &lookup::Options {
                offset: 1,
                limit: 2,
                max_evidence: 1,
            },
        )
        .unwrap();
    assert!(page.truncation.truncated);
    assert_eq!(page.truncation.groups_total, full.readings.len());
    assert_eq!(page.readings.len(), 2);
    assert_eq!(page.readings[0].normalized, full.readings[1].normalized);
    // Статус и полный набор чтений не зависят от страницы.
    assert_eq!(page.status, full.status);
    assert_eq!(page.distinct_readings, full.distinct_readings);
    let nama = lexicon
        .lookup(
            "生",
            &lookup::Options {
                offset: 0,
                limit: 50,
                max_evidence: 1,
            },
        )
        .unwrap();
    assert!(nama.readings.iter().all(|g| g.evidence.len() <= 1));
    // Кандидаты строятся только по показанной странице.
    let full_targets: Vec<_> = full
        .candidate_targets()
        .into_iter()
        .map(|t| (t.surface, t.reading))
        .collect();
    assert_eq!(
        full_targets,
        vec![("生".into(), "せい".into()), ("生".into(), "なま".into())]
    );
    let first = lexicon
        .lookup(
            "生",
            &lookup::Options {
                offset: 0,
                limit: 1,
                max_evidence: 10,
            },
        )
        .unwrap();
    let paged: Vec<_> = first
        .candidate_targets()
        .into_iter()
        .map(|t| (t.surface, t.reading))
        .collect();
    assert_eq!(paged.len(), 1);
    assert!(full_targets.contains(&paged[0]));
    let hidden: usize = full
        .readings
        .iter()
        .map(|g| g.evidence.len().saturating_sub(1))
        .sum();
    assert!(
        hidden > 0,
        "фикстура должна давать группу с несколькими свидетельствами"
    );
    assert_eq!(nama.truncation.evidence_omitted, hidden);
    assert!(nama.truncation.truncated);
    assert!(lexicon.lookup("", &lookup::Options::default()).is_err());
}

#[test]
fn unknown_registry_entry_is_reported() {
    let tmp = TempDir::new("unknown-entry");
    let root = tmp.path().join("data");
    let (loc, _) = installed(&root);
    let path = root.join(store::REGISTRY_FILE);
    let mut registry: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let entry = registry["dictionaries"]["jitendex"].clone();
    registry["dictionaries"]["mystery"] = entry;
    std::fs::write(&path, serde_json::to_vec(&registry).unwrap()).unwrap();

    let verify = manager::verify(&loc, &all()).unwrap();
    assert!(!verify.ok);
    assert_eq!(verify.unknown_registry_entries, vec!["mystery"]);
    assert!(verify.dictionaries.iter().all(|d| d.state == State::Ok));
}

#[test]
fn empty_data_dir_is_honest() {
    let tmp = TempDir::new("empty");
    let root = tmp.path().join("never-created");
    let r = lookup(&root, "学校");
    assert_eq!(r.status, Status::SourceMissing);
    // KANJIDIC относится только к запросу одиночного кандзи.
    assert_eq!(
        r.missing_sources,
        vec!["jitendex", "jmnedict", "jpdb-v2.2-freq-kana"]
    );
    assert_eq!(lookup(&root, "生").missing_sources.len(), 4);
    assert_eq!(lookup(&root, "かな").status, Status::SingleReading);
    assert_eq!(lookup(&root, "ｶﾅ").status, Status::Ambiguous);
    let status = manager::status(&location(&root)).unwrap();
    assert!(!status.exists);
    assert!(!root.exists(), "status не создаёт каталог");
    let up = FakeUpstream::standard();
    let check = manager::check_updates(&root, &all(), &up).unwrap();
    assert!(
        check
            .items
            .iter()
            .all(|i| i.outcome == Outcome::NotInstalled && i.remote.is_some())
    );
    assert!(!root.exists(), "check-updates не создаёт каталог");
}

/// Полный перечень записей дерева, включая файлы замков и ссылки.
fn listing(root: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                stack.push(path.clone());
            }
            out.push(path.strip_prefix(root).unwrap().to_path_buf());
        }
    }
    out.sort();
    out
}

/// Внешний каталог с «чужими» файлами, которые не должны пострадать.
fn victim(tmp: &TempDir, name: &str) -> std::path::PathBuf {
    let dir = tmp.path().join(name);
    std::fs::create_dir_all(dir.join("nested")).unwrap();
    std::fs::write(dir.join("precious.txt"), b"keep me").unwrap();
    std::fs::write(dir.join("nested/also.txt"), b"keep me too").unwrap();
    dir
}

#[test]
fn symlinked_managed_directories_are_refused_without_touching_targets() {
    use std::os::unix::fs::symlink;
    let up = FakeUpstream::standard();
    up.publish("jitendex", "T2", jitendex("T2", &[]));
    for link in ["staging", "dictionaries", "dictionaries/jitendex"] {
        let tmp = TempDir::new("symlink");
        let root = tmp.path().join("data");
        let (loc, _) = installed(&root);
        let outside = victim(&tmp, "outside");
        let before = snapshot(&outside);
        let managed = root.join(link);
        let moved = tmp.path().join("moved");
        std::fs::rename(&managed, &moved).unwrap();
        symlink(&outside, &managed).unwrap();

        let install = manager::install(&root, &all(), &up, None, &Options::default());
        assert_eq!(install.unwrap_err().code, Code::NotOwned, "{link}: install");
        let update = manager::update(&root, &all(), &up, &Options::default());
        assert_eq!(update.unwrap_err().code, Code::NotOwned, "{link}: update");
        let remove = manager::remove(&root, &all());
        assert_eq!(remove.unwrap_err().code, Code::NotOwned, "{link}: remove");
        assert_eq!(
            manager::status(&loc).unwrap_err().code,
            Code::NotOwned,
            "{link}: status"
        );
        assert_eq!(
            manager::verify(&loc, &all()).unwrap_err().code,
            Code::NotOwned,
            "{link}: verify"
        );
        assert_eq!(snapshot(&outside), before, "{link}: внешние файлы изменены");
        assert!(
            std::fs::symlink_metadata(&managed)
                .unwrap()
                .file_type()
                .is_symlink(),
            "{link}: ссылка не должна заменяться"
        );
        // Реестр не тронут отказавшим remove.
        assert_eq!(
            Store::open_existing(&root)
                .unwrap()
                .unwrap()
                .registry()
                .unwrap()
                .dictionaries
                .len(),
            4
        );
        // После восстановления настоящего каталога всё снова работает.
        std::fs::remove_file(&managed).unwrap();
        std::fs::rename(&moved, &managed).unwrap();
        assert!(manager::verify(&loc, &all()).unwrap().ok, "{link}");
    }

    // Файл на месте управляемого каталога — тоже отказ, без удаления.
    let tmp = TempDir::new("symlink-file");
    let root = tmp.path().join("data");
    installed(&root);
    std::fs::remove_dir_all(root.join("staging")).unwrap();
    std::fs::write(root.join("staging"), b"not a dir").unwrap();
    let err = manager::update(&root, &all(), &up, &Options::default()).unwrap_err();
    assert_eq!(err.code, Code::NotOwned);
    assert_eq!(std::fs::read(root.join("staging")).unwrap(), b"not a dir");

    // Ссылка на месте остатка (листовой каталог версии) убирается как
    // ссылка: цель не обходится, обычное восстановление работает.
    let tmp = TempDir::new("symlink-leaf");
    let root = tmp.path().join("data");
    installed(&root);
    let outside = victim(&tmp, "outside");
    let before = snapshot(&outside);
    symlink(&outside, root.join("staging/0123456789abcdef")).unwrap();
    symlink(
        &outside,
        root.join("dictionaries/jitendex/0000000000000000-00000000"),
    )
    .unwrap();
    let update = manager::update(&root, &all(), &up, &Options::default()).unwrap();
    assert!(update.ok, "{update:#?}");
    assert_eq!(update.cleaned.len(), 2);
    assert!(!root.join("staging/0123456789abcdef").exists());
    assert_eq!(snapshot(&outside), before);
}

#[test]
fn read_only_commands_create_no_files() {
    let tmp = TempDir::new("read-only");
    let root = tmp.path().join("data");
    let (loc, up) = installed(&root);
    // Каталог, принадлежащий подсистеме, но без файлов замков (например,
    // после ручной уборки): команды чтения не должны их создавать.
    std::fs::remove_file(root.join(store::ACTIVE_LOCK)).unwrap();
    std::fs::remove_file(root.join(store::OPERATIONS_LOCK)).unwrap();
    let before = listing(&root);
    let contents = snapshot(&root);

    assert!(manager::status(&loc).unwrap().ok);
    assert_eq!(listing(&root), before, "status");
    assert!(manager::verify(&loc, &all()).unwrap().ok);
    assert_eq!(listing(&root), before, "verify");
    assert_eq!(lookup(&root, "学校").status, Status::SingleReading);
    assert_eq!(listing(&root), before, "lookup");
    let check = manager::check_updates(&root, &all(), &up).unwrap();
    assert!(check.ok, "{check:#?}");
    assert_eq!(listing(&root), before, "check-updates");
    assert_eq!(snapshot(&root), contents);
    assert!(!root.join(store::ACTIVE_LOCK).exists());

    // Изменяющая операция создаёт замок до того, как начнёт менять версии.
    up.publish("jitendex", "T2", jitendex("T2", &[]));
    manager::update(&root, &one("jitendex"), &up, &Options::default()).unwrap();
    assert!(root.join(store::ACTIVE_LOCK).exists());

    // Свежая инициализация тоже создаёт замок, а чтение после неё — нет.
    let fresh = tmp.path().join("fresh");
    let report = manager::remove(&fresh, &all()).unwrap();
    assert!(
        report
            .items
            .iter()
            .all(|i| i.outcome == Outcome::NotInstalled)
    );
    assert!(!fresh.exists(), "remove без каталога ничего не создаёт");
}

#[test]
fn concurrent_reads_stay_consistent_during_activation_and_removal() {
    let tmp = TempDir::new("read-race");
    let root = tmp.path().join("data");
    let (loc, up) = installed(&root);
    // Начинаем без файла замка: путь чтения без блокировки с повтором.
    std::fs::remove_file(root.join(store::ACTIVE_LOCK)).unwrap();
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        let readers: Vec<_> = (0..3)
            .map(|_| {
                scope.spawn(|| {
                    let mut rounds = 0;
                    while !stop.load(Ordering::SeqCst) {
                        let lexicon = Lexicon::open(&root).unwrap();
                        let r = lexicon.lookup("学校", &lookup::Options::default()).unwrap();
                        for s in &r.sources {
                            assert_ne!(
                                s.state,
                                lookup::SourceState::Unavailable,
                                "версия исчезла между чтением реестра и открытием: {s:?}"
                            );
                        }
                        let status = manager::status(&loc).unwrap();
                        assert!(
                            status
                                .dictionaries
                                .iter()
                                .all(|d| d.state != State::Corrupt),
                            "{status:#?}"
                        );
                        rounds += 1;
                    }
                    rounds
                })
            })
            .collect();
        for n in 2..6 {
            let release = format!("T{n}");
            up.publish("jitendex", &release, jitendex(&release, &[]));
            let r = manager::update(&root, &one("jitendex"), &up, &Options::default()).unwrap();
            assert_eq!(outcome(&r, "jitendex"), Outcome::Updated);
        }
        manager::remove(&root, &one("jitendex")).unwrap();
        let r = manager::install(&root, &one("jitendex"), &up, None, &Options::default()).unwrap();
        assert!(r.ok);
        stop.store(true, Ordering::SeqCst);
        for reader in readers {
            assert!(reader.join().unwrap() > 0);
        }
    });
}

#[test]
fn exact_lookup_keys_are_nfc_on_import_and_query() {
    let tmp = TempDir::new("nfc");
    let root = tmp.path().join("data");
    let up = FakeUpstream::standard();
    // ドア с разложенным дакутэном (ト + U+3099) и пустым чтением;
    // パン — составной формой.
    let decomposed = "\u{30C8}\u{3099}\u{30A2}";
    up.publish(
        "jitendex",
        "T1",
        jitendex(
            "T1",
            &[
                json!([decomposed, "", "", "", 10, [{"type": "structured-content", "content": "door"}], 100, ""]),
                json!(["パン", "", "", "", 10, [{"type": "structured-content", "content": "bread"}], 101, ""]),
            ],
        ),
    );
    // KANJIDIC: совместимый иероглиф U+FA10 канонически равен 塚 (U+585A).
    up.publish(
        "kanjidic-english",
        "2026-01-01",
        zip_bytes(vec![
            file(
                "index.json",
                &json!({"title": "KANJIDIC [T1]", "format": 3, "revision": "T1"}),
            ),
            file(
                "kanji_bank_1.json",
                &json!([["\u{FA10}", "チョウ", "つか", "", ["mound"], {}]]),
            ),
        ]),
    );
    // JPDB: частотность записи ドア тоже в разложенной форме.
    up.publish(
        "jpdb-v2.2-freq-kana",
        "2024-10-13",
        zip_bytes(vec![
            file(
                "index.json",
                &json!({"title": "JPDBv2㋕", "format": 3, "revision": "T1", "frequencyMode": "rank-based"}),
            ),
            file(
                "term_meta_bank_1.json",
                &json!([[decomposed, "freq", {"value": 900, "displayValue": "900㋕"}]]),
            ),
        ]),
    );
    let report = manager::install(&root, &all(), &up, None, &Options::default()).unwrap();
    assert!(report.ok, "{report:#?}");

    let composed = lookup(&root, "ドア");
    let raw = lookup(&root, decomposed);
    assert_eq!(
        serde_json::to_value(&composed.readings).unwrap(),
        serde_json::to_value(&raw.readings).unwrap(),
        "составная и разложенная форма запроса дают одно извлечение"
    );
    assert!(raw.query.nfc_changed);
    let lexical = composed
        .readings
        .iter()
        .find(|g| g.class == EvidenceClass::LexicalEntry)
        .expect("статья с разложенным написанием находится по NFC-запросу");
    assert_eq!(lexical.normalized, "どあ");
    // Происхождение — исходная строка словаря и её адрес в архиве.
    let evidence = &lexical.evidence[0];
    assert_eq!(evidence.surface, decomposed);
    assert_eq!(evidence.origin, "kana_term");
    let entry = evidence.entry.as_ref().unwrap();
    assert_eq!(entry.bank, "term_bank_1.json");
    assert_eq!(entry.sequence, Some(100));
    // Частотность с разложенным ключом аннотирует ту же группу.
    assert_eq!(lexical.frequency.len(), 1);
    assert_eq!(lexical.frequency[0].display.as_deref(), Some("900㋕"));
    assert!(composed.frequency_unmatched.is_empty());

    // Составная форма в источнике находится разложенным запросом.
    let pan = lookup(&root, "\u{30CF}\u{309A}\u{30F3}");
    assert!(
        pan.readings
            .iter()
            .any(|g| g.class == EvidenceClass::LexicalEntry && g.normalized == "ぱん")
    );

    // Банк кандзи: совместимый иероглиф канонически равен основному.
    let tsuka = lookup(&root, "\u{585A}");
    let kanji = tsuka
        .readings
        .iter()
        .find(|g| g.class == EvidenceClass::KanjiReference)
        .expect("KANJIDIC находит канонически эквивалентный знак");
    assert_eq!(kanji.evidence[0].surface, "\u{FA10}");

    // NFKC-эквивалентная, но канонически иная запись не сливается.
    let halfwidth = lookup(&root, "ﾄﾞｱ");
    assert!(
        halfwidth
            .readings
            .iter()
            .all(|g| g.class != EvidenceClass::LexicalEntry)
    );
    assert_eq!(halfwidth.status, Status::Ambiguous);
}

#[test]
fn zero_page_limits_are_rejected_explicitly() {
    let tmp = TempDir::new("zero-limits");
    let root = tmp.path().join("data");
    installed(&root);
    let lexicon = Lexicon::open(&root).unwrap();
    for options in [
        lookup::Options {
            limit: 0,
            ..lookup::Options::default()
        },
        lookup::Options {
            max_evidence: 0,
            ..lookup::Options::default()
        },
    ] {
        let err = lexicon.lookup("生", &options).unwrap_err();
        assert_eq!(err.code, Code::InvalidInput, "{options:?}");
    }
    let one_each = lexicon
        .lookup(
            "生",
            &lookup::Options {
                offset: 0,
                limit: 1,
                max_evidence: 1,
            },
        )
        .unwrap();
    assert_eq!(one_each.truncation.limit, 1);
    assert_eq!(one_each.readings.len(), 1);
    assert_eq!(one_each.readings[0].evidence.len(), 1);
}
