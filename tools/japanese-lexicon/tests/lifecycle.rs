//! Жизненный цикл словарей на синтетических архивах и поддельном источнике публикации.

mod common;

use common::*;
use japanese_lexicon::catalog;
use japanese_lexicon::error::Code;
use japanese_lexicon::index::BuildStats;
use japanese_lexicon::lookup::{self, EvidenceClass, Lexicon, SourceState, Status};
use japanese_lexicon::manager::{self, Options, Outcome, State};
use japanese_lexicon::store::{self, Location, Origin, Store};
use japanese_lexicon::yomitan::Limits;
use serde_json::{Value, json};
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

/// Имя активной версии словаря из реестра.
fn version_name(root: &Path, id: &str) -> String {
    Store::open_existing(root)
        .unwrap()
        .unwrap()
        .registry()
        .unwrap()
        .dictionaries[id]
        .version
        .clone()
}

/// Каталог активной версии установленного словаря.
fn version_dir(root: &Path, id: &str) -> std::path::PathBuf {
    root.join("dictionaries")
        .join(id)
        .join(version_name(root, id))
}

/// Подменяет архив активной версии и пересобирает её индекс и манифест:
/// воспроизводит состояние, установленное до появления проверок полноты.
/// Манифест обновляется вместе с индексом, иначе проверка откажет по другой
/// причине — расхождению с записанными размерами и суммой.
fn replace_active_version(root: &Path, id: &str, bytes: Vec<u8>) -> BuildStats {
    let source = catalog::find(id).unwrap();
    let dir = version_dir(root, id);
    let archive = dir.join(store::ARCHIVE_FILE);
    std::fs::write(&archive, bytes).unwrap();
    let archive_digest = japanese_lexicon::upstream::digest_file(&archive).unwrap();
    let (mut opened, inspection) =
        japanese_lexicon::yomitan::Archive::open(&archive, source, Limits::default()).unwrap();
    let index_path = dir.join(store::INDEX_FILE);
    std::fs::remove_file(&index_path).unwrap();
    let meta = std::collections::BTreeMap::from([(
        "archive_sha256".to_owned(),
        archive_digest.sha256.clone(),
    )]);
    let stats =
        japanese_lexicon::index::build(&mut opened, &inspection, source, &index_path, &meta)
            .unwrap();
    let store = Store::open_existing(root).unwrap().unwrap();
    let version = version_name(root, id);
    let mut manifest = store.read_manifest(id, &version).unwrap();
    let index_digest = japanese_lexicon::upstream::digest_file(&index_path).unwrap();
    manifest.index_json = inspection.index;
    manifest.archive = archive_digest;
    manifest.corpus.bytes = index_digest.bytes;
    manifest.corpus.sha256 = index_digest.sha256;
    manifest.corpus.stats = stats.clone();
    store::write_atomic(
        &dir.join(store::MANIFEST_FILE),
        &serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    stats
}

/// Адресная порча установленного индекса: таблица удаляется, сам файл остаётся
/// корректной базой, поэтому словарь открывается и отказывает уже в поиске.
fn drop_table(index: &Path, table: &str) {
    let conn = rusqlite::Connection::open(index).unwrap();
    conn.execute(&format!("DROP TABLE {table}"), []).unwrap();
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

    // Помета с дефисами с обеих сторон — значение `infix` машинного контракта;
    // у 間 есть и чтения знака, и вариант имени собственного.
    let r = lookup(&root, "間");
    assert_eq!(r.status, Status::KanjiReferenceAndProperNameOnly);
    assert!(r.distinct_readings.is_empty());
    let ma = r
        .readings
        .iter()
        .find(|g| g.normalized == "ま")
        .expect("нет чтения символа ま");
    assert_eq!(ma.class, EvidenceClass::KanjiReference);
    assert_eq!(ma.evidence[0].affix, Some("infix"));
    let name = r
        .readings
        .iter()
        .find(|g| g.class == EvidenceClass::ProperName)
        .expect("нет варианта имени собственного");
    assert_eq!(name.normalized, "あいだ");

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
        (
            // Обязательного банка нет вовсе: отклоняется ещё при осмотре архива.
            "no_bank",
            zip_bytes(vec![
                file(
                    "index.json",
                    &json!({"title": "Jitendex.org [T9]", "format": 3, "revision": "T9"}),
                ),
                file("tag_bank_1.json", &json!([])),
            ]),
            Code::InvalidFormat,
        ),
        (
            // Обязательный банк есть, но пуст: структура верна, словаря нет.
            "empty_bank",
            zip_bytes(vec![
                file(
                    "index.json",
                    &json!({"title": "Jitendex.org [T9]", "format": 3, "revision": "T9"}),
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

/// Пустой обязательный банк не даёт установить словарь ни одного типа:
/// статьи для Jitendex и JMnedict, кандзи для KANJIDIC, частотные
/// метаданные для JPDB. Проверка идёт до манифеста и до активации, а
/// диагностика называет вид банка по-русски и точным шаблоном Yomitan.
#[test]
fn empty_required_bank_is_refused_for_every_source() {
    let cases = [
        (
            "jitendex",
            "T1",
            empty_dictionary("Jitendex.org [T1]", "T1", "term_bank_1.json"),
            "нет ни одной записи обязательного банка статей (term_bank_*.json)",
        ),
        (
            "jmnedict",
            "2026-01-01",
            empty_dictionary(
                "JMnedict [2026-01-01]",
                "JMnedict.2026-01-01",
                "term_bank_1.json",
            ),
            "нет ни одной записи обязательного банка статей (term_bank_*.json)",
        ),
        (
            "kanjidic-english",
            "2026-01-01",
            empty_dictionary(
                "KANJIDIC [kanjidic2.test]",
                "kanjidic2.test",
                "kanji_bank_1.json",
            ),
            "нет ни одной записи обязательного банка кандзи (kanji_bank_*.json)",
        ),
        (
            "jpdb-v2.2-freq-kana",
            "2024-10-13",
            jpdb_rows("JPDB v2.2 Frequency Kana 2024-10-13", &[]),
            "нет ни одной записи обязательного банка метаданных терминов (term_meta_bank_*.json)",
        ),
        (
            // Обязательного банка кандзи нет вовсе: отказ ещё при осмотре
            // архива, до построения индекса. Разрешённый банк метаданных при
            // этом непуст — решает обязательность, а не имя файла.
            "kanjidic-english",
            "2026-01-01",
            zip_bytes(vec![
                file(
                    "index.json",
                    &json!({"title": "KANJIDIC [kanjidic2.test]", "format": 3, "revision": "kanjidic2.test"}),
                ),
                file(
                    "kanji_meta_bank_1.json",
                    &json!([["生", "freq", {"value": 1, "displayValue": "1"}]]),
                ),
            ]),
            "нет обязательного банка кандзи (kanji_bank_*.json)",
        ),
    ];
    for (id, release, bytes, expected) in cases {
        let tmp = TempDir::new("empty-bank");
        let root = tmp.path().join("data");
        let up = FakeUpstream::standard();
        up.publish(id, release, bytes);
        let report = manager::install(&root, &one(id), &up, None, &Options::default()).unwrap();
        assert_eq!(outcome(&report, id), Outcome::Failed, "{id}: {report:#?}");
        assert_eq!(code(&report, id), Some(Code::InvalidFormat), "{id}");
        // Внутреннее имя варианта `BankKind` в тексте недопустимо: смысл
        // банка пользователю объясняют вид и шаблон файла.
        let message = &report.items[0].message;
        assert!(message.contains(expected), "{id}: {message}");
        for internal in ["Term", "TermMeta", "Kanji", "Tag"] {
            assert!(!message.contains(internal), "{id}: {message}");
        }
        assert!(report.items[0].installed.is_none(), "{id}");
        // Ни версии, ни записи реестра, ни остатков сборки.
        assert!(!root.join("dictionaries").join(id).exists(), "{id}");
        let store = Store::open_existing(&root).unwrap().unwrap();
        assert!(
            !store.registry().unwrap().dictionaries.contains_key(id),
            "{id}"
        );
        assert!(
            std::fs::read_dir(root.join("staging"))
                .unwrap()
                .next()
                .is_none(),
            "{id}: staging"
        );
        let status = manager::status(&location(&root)).unwrap();
        let item = status.dictionaries.iter().find(|d| d.id == id).unwrap();
        assert_eq!(item.state, State::NotInstalled, "{id}");
        assert!(item.installed.is_none(), "{id}");
    }
}

/// Пустой необязательный банк рядом с непустым обязательным — допустим:
/// словарь содержит записи, а проверка не превращается в запрет пустых
/// банков вообще.
#[test]
fn empty_optional_bank_does_not_block_install() {
    let tmp = TempDir::new("empty-optional");
    let root = tmp.path().join("data");
    let up = FakeUpstream::standard();
    up.publish(
        "jitendex",
        "T1",
        zip_bytes(vec![
            file(
                "index.json",
                &json!({"title": "Jitendex.org [T1]", "format": 3, "revision": "T1"}),
            ),
            file(
                "term_bank_1.json",
                &json!([["学校", "がっこう", "", "", 1, ["school"], 1, ""]]),
            ),
            file("term_bank_2.json", &json!([])),
            file("tag_bank_1.json", &json!([])),
        ]),
    );
    let report = manager::install(&root, &one("jitendex"), &up, None, &Options::default()).unwrap();
    assert_eq!(
        outcome(&report, "jitendex"),
        Outcome::Installed,
        "{report:#?}"
    );
    assert!(manager::verify(&location(&root), &all()).unwrap().ok);
    assert_eq!(lookup(&root, "学校").status, Status::SingleReading);
}

/// Полноценный лексический словарь обязан давать пригодные чтения: архив со
/// статьями, из которых не выводится ни одного чтения, не устанавливается.
/// Иначе содержательно неполный артефакт заменил бы рабочий словарь.
#[test]
fn lexicon_without_a_single_reading_is_refused() {
    let cases = [
        (
            "jitendex",
            "T1",
            jitendex_rows(
                "T1",
                // Кандзи без чтения: строка банка есть, чтений она не даёт.
                &[json!(["謎字", "", "", "", 0, ["mystery"], 1, ""])],
            ),
        ),
        (
            "jmnedict",
            "2026-01-01",
            zip_bytes(vec![
                file(
                    "index.json",
                    &json!({"title": "JMnedict [2026-01-01]", "format": 3,
                            "revision": "JMnedict.2026-01-01"}),
                ),
                // Пустое чтение имени: глоссарий не список чтений каной.
                file(
                    "term_bank_1.json",
                    &json!([["謎字", "", "surname", "", 0, ["mystery"], 1, ""]]),
                ),
            ]),
        ),
    ];
    for (id, release, bytes) in cases {
        let tmp = TempDir::new("no-readings");
        let root = tmp.path().join("data");
        let up = FakeUpstream::standard();
        up.publish(id, release, bytes);
        let report = manager::install(&root, &one(id), &up, None, &Options::default()).unwrap();
        assert_eq!(outcome(&report, id), Outcome::Failed, "{id}: {report:#?}");
        assert_eq!(code(&report, id), Some(Code::InvalidFormat), "{id}");
        let message = &report.items[0].message;
        assert!(
            message.contains("нет ни одного пригодного чтения"),
            "{id}: {message}"
        );
        // Диагностика называет банк по-русски и точно, а не внутренним
        // именем варианта `BankKind`.
        assert!(
            message.contains("статей (term_bank_*.json)"),
            "{id}: {message}"
        );
        for internal in ["Term", "TermMeta", "Kanji", "Tag"] {
            assert!(!message.contains(internal), "{id}: {message}");
        }
        assert!(report.items[0].installed.is_none(), "{id}");
        // Ни версии, ни записи реестра, ни остатков сборки.
        assert!(!root.join("dictionaries").join(id).exists(), "{id}");
        let store = Store::open_existing(&root).unwrap().unwrap();
        assert!(
            !store.registry().unwrap().dictionaries.contains_key(id),
            "{id}"
        );
        assert!(
            std::fs::read_dir(root.join("staging"))
                .unwrap()
                .next()
                .is_none(),
            "{id}: staging"
        );
    }
}

/// Отдельные строки без чтений внутри пригодного корпуса легальны: правило о
/// пригодности корпуса в целом, а не о каждой записи. Штатная фикстура
/// Jitendex содержит и статью кандзи без чтения, и указатель старой формы.
#[test]
fn readingless_rows_inside_a_usable_corpus_are_accepted() {
    let tmp = TempDir::new("readingless-rows");
    let root = tmp.path().join("data");
    let up = FakeUpstream::standard();
    let report = manager::install(&root, &one("jitendex"), &up, None, &Options::default()).unwrap();
    assert_eq!(
        outcome(&report, "jitendex"),
        Outcome::Installed,
        "{report:#?}"
    );
    let stats = &report.items[0].installed.as_ref().unwrap().stats;
    assert!(stats.readings > 0, "{stats:?}");
    assert!(stats.terms_without_reading > 0, "{stats:?}");
    assert!(manager::verify(&location(&root), &all()).unwrap().ok);
    assert_eq!(lookup(&root, "学校").status, Status::SingleReading);
}

/// Источники, не выводящие чтения, проверку чтений не получают: KANJIDIC
/// говорит о чтениях отдельного знака, JPDB — аннотация частотности. Общая
/// проверка `readings > 0` запретила бы оба словаря целиком.
#[test]
fn sources_that_do_not_derive_readings_are_not_required_to_have_them() {
    let tmp = TempDir::new("kanji-frequency");
    let root = tmp.path().join("data");
    let up = FakeUpstream::standard();
    up.publish(
        "kanjidic-english",
        "2026-01-01",
        zip_bytes(vec![
            file(
                "index.json",
                &json!({"title": "KANJIDIC [kanjidic2.test]", "format": 3,
                        "revision": "kanjidic2.test"}),
            ),
            // Знак без указанных он/кун-чтений: счётчик чтений корпуса пуст.
            file(
                "kanji_bank_1.json",
                &json!([["學", "", "", "jouyou", ["study"], {"grade": "8"}]]),
            ),
        ]),
    );
    up.publish(
        "jpdb-v2.2-freq-kana",
        "2024-10-13",
        jpdb_rows(
            "JPDB v2.2 Frequency Kana 2024-10-13",
            &[json!(["学校", "freq", {"value": 616, "displayValue": "616"}])],
        ),
    );
    for (id, revision) in [
        ("kanjidic-english", "kanjidic2.test"),
        ("jpdb-v2.2-freq-kana", "JPDB v2.2 Frequency Kana 2024-10-13"),
    ] {
        let report = manager::install(&root, &one(id), &up, None, &Options::default()).unwrap();
        assert_eq!(
            outcome(&report, id),
            Outcome::Installed,
            "{id}: {report:#?}"
        );
        let installed = report.items[0].installed.as_ref().unwrap();
        assert_eq!(installed.stats.readings, 0, "{id}");
        assert_eq!(installed.revision, revision, "{id}");
    }
    assert!(manager::verify(&location(&root), &all()).unwrap().ok);
}

/// Корпус с записями, но без единого чтения, установленный до появления
/// проверки, остаётся активным, но `status` и `verify` обязаны его
/// обнаружить, а `update` — починить, а не объявить актуальным.
#[test]
fn installed_corpus_without_readings_is_reported_and_repaired_by_update() {
    let tmp = TempDir::new("no-readings-installed");
    let root = tmp.path().join("data");
    let (loc, up) = installed(&root);
    let stats = replace_active_version(
        &root,
        "jitendex",
        jitendex_rows("T1", &[json!(["謎字", "", "", "", 0, ["mystery"], 1, ""])]),
    );
    assert_eq!((stats.terms, stats.readings), (1, 0));

    for report in [
        manager::status(&loc).unwrap(),
        manager::verify(&loc, &all()).unwrap(),
    ] {
        assert!(!report.ok, "неполный словарь не должен считаться исправным");
        let item = report
            .dictionaries
            .iter()
            .find(|d| d.id == "jitendex")
            .unwrap();
        assert_eq!(item.state, State::Corrupt);
        assert!(
            item.problems.iter().any(|p| p.code == Code::InvalidFormat
                && p.message.contains("нет ни одного пригодного чтения")
                && p.message.contains("term_bank_*.json")),
            "{:#?}",
            item.problems
        );
    }

    let update = manager::update(&root, &one("jitendex"), &up, &Options::default()).unwrap();
    assert_eq!(
        outcome(&update, "jitendex"),
        Outcome::Updated,
        "{update:#?}"
    );
    assert!(manager::verify(&loc, &all()).unwrap().ok);
    assert_eq!(lookup(&root, "学校").status, Status::SingleReading);
}

/// Аномально сокращённый корпус не активируется: отказ происходит до
/// манифеста и активации, поэтому прежняя исправная версия остаётся активной,
/// ищется, проверяется и не теряет ни одного файла, а её реестр не меняется.
#[test]
fn update_to_an_anomalously_shrunken_lexicon_keeps_the_working_version() {
    let tmp = TempDir::new("shrunk-lexicon");
    let root = tmp.path().join("data");
    let (loc, up) = installed(&root);
    let good = snapshot(&root.join("dictionaries"));
    let active = version_dir(&root, "jitendex");

    // Формально корректный архив: статьи есть, чтения есть, но корпус усечён
    // более чем вдвое против прежней исправной версии.
    up.publish("jitendex", "T2", lexicon_archive("T2", &LEXICON_PAIRS[..2]));
    let update = manager::update(&root, &one("jitendex"), &up, &Options::default()).unwrap();
    assert_eq!(outcome(&update, "jitendex"), Outcome::Failed, "{update:#?}");
    assert_eq!(code(&update, "jitendex"), Some(Code::UpstreamUnexpected));
    let previous = update.items[0].installed.as_ref().unwrap();
    let message = &update.items[0].message;
    assert!(message.contains("аномально меньше"), "{message}");
    // Диагностика называет обе сопоставимые величины, прежнюю версию и
    // фактическое последствие отказа.
    for expected in [
        format!(
            "записей обязательных банков 2 против {}",
            previous.stats.terms
        ),
        format!("пригодных чтений 2 против {}", previous.stats.readings),
        previous.revision.clone(),
        "активная версия сохранена".to_owned(),
    ] {
        assert!(message.contains(&expected), "{expected}: {message}");
    }
    // Прежняя версия: те же файлы, тот же каталог версии, пустой staging.
    assert_eq!(snapshot(&root.join("dictionaries")), good);
    assert_eq!(version_dir(&root, "jitendex"), active);
    assert!(
        std::fs::read_dir(root.join("staging"))
            .unwrap()
            .next()
            .is_none(),
        "staging"
    );
    // Словарь по-прежнему доступен для поиска и проходит проверку.
    let r = lookup(&root, "学校");
    assert_eq!(r.status, Status::SingleReading);
    assert_eq!(
        r.readings[0].evidence[0]
            .dictionary
            .as_ref()
            .unwrap()
            .revision,
        "T1"
    );
    assert!(
        r.sources
            .iter()
            .find(|s| s.id == "jitendex")
            .unwrap()
            .dictionary
            .is_some()
    );
    assert!(manager::verify(&loc, &all()).unwrap().ok);
    // Отказ не изменил оценку активной версии: она по-прежнему исправна.
    assert!(manager::status(&loc).unwrap().ok);

    // Повтор обновления терпит тот же отказ: кандидат не становится активным.
    let again = manager::update(&root, &one("jitendex"), &up, &Options::default()).unwrap();
    assert_eq!(outcome(&again, "jitendex"), Outcome::Failed, "{again:#?}");
    assert_eq!(snapshot(&root.join("dictionaries")), good);
    // Обновление честно видно как доступное, а не как «нет обновлений».
    let check = manager::check_updates(&root, &one("jitendex"), &up).unwrap();
    assert_eq!(outcome(&check, "jitendex"), Outcome::UpdateAvailable);
}

/// Сокращение одних чтений при прежнем числе статей — отдельное условие отказа:
/// статьи целы, поэтому сравнение записей не срабатывает и решение принимает
/// только показатель чтений. Без такого кандидата это условие проверялось бы
/// лишь косвенно — усечение статей уже даёт отказ само по себе.
#[test]
fn update_that_only_shrinks_readings_is_refused_by_its_own_measure() {
    let tmp = TempDir::new("shrunk-readings");
    let root = tmp.path().join("data");
    let (loc, up) = installed(&root);
    let good = snapshot(&root.join("dictionaries"));
    // Статей столько же, сколько у активной версии; чтения есть у двух из них,
    // у остальных строк чтение пустое — банк статей цел, чтений меньше половины.
    let rows: Vec<Value> = LEXICON_PAIRS
        .iter()
        .enumerate()
        .map(|(i, (surface, reading))| {
            let reading = if i < 2 { *reading } else { "" };
            json!([surface, reading, "", "", 200, ["meaning"], 1, ""])
        })
        .collect();
    up.publish("jitendex", "T2", jitendex_rows("T2", &rows));
    let update = manager::update(&root, &one("jitendex"), &up, &Options::default()).unwrap();
    assert_eq!(outcome(&update, "jitendex"), Outcome::Failed, "{update:#?}");
    assert_eq!(code(&update, "jitendex"), Some(Code::UpstreamUnexpected));
    let previous = update.items[0].installed.as_ref().unwrap();
    let message = &update.items[0].message;
    assert!(message.contains("аномально меньше"), "{message}");
    assert!(
        message.contains(&format!(
            "пригодных чтений 2 против {}",
            previous.stats.readings
        )),
        "{message}"
    );
    // Отказ вызван именно чтениями: записей не меньше, чем было.
    assert!(
        !message.contains("записей обязательных банков"),
        "{message}"
    );
    // Прежняя версия цела и остаётся активной для поиска.
    assert_eq!(snapshot(&root.join("dictionaries")), good);
    assert!(manager::verify(&loc, &all()).unwrap().ok);
    assert!(manager::status(&loc).unwrap().ok);
    assert_eq!(lookup(&root, "学校").status, Status::SingleReading);
}

/// Показатели сравнения берутся по роли словаря: у KANJIDIC корпус задают
/// знаки, и чтений в сравнении нет вовсе — иначе источник с нулём чтений
/// сравнивался бы по чужим для него данным.
#[test]
fn update_to_a_shrunken_kanji_corpus_is_refused_by_its_own_measure() {
    let tmp = TempDir::new("shrunk-kanji");
    let root = tmp.path().join("data");
    let (loc, up) = installed(&root);
    let good = snapshot(&root.join("dictionaries"));
    // Один знак вместо трёх: банк кандзи усечён, чтений у источника и не было.
    up.publish(
        "kanjidic-english",
        "2026-01-01",
        zip_bytes(vec![
            file(
                "index.json",
                &json!({"title": "KANJIDIC [kanjidic2.test]", "format": 3,
                        "revision": "kanjidic2.test"}),
            ),
            file(
                "kanji_bank_1.json",
                &json!([["學", "", "", "jouyou", ["study"], {"grade": "8"}]]),
            ),
        ]),
    );
    let update =
        manager::update(&root, &one("kanjidic-english"), &up, &Options::default()).unwrap();
    assert_eq!(
        outcome(&update, "kanjidic-english"),
        Outcome::Failed,
        "{update:#?}"
    );
    assert_eq!(
        code(&update, "kanjidic-english"),
        Some(Code::UpstreamUnexpected)
    );
    let message = &update.items[0].message;
    assert!(
        message.contains("записей обязательных банков 1 против 3"),
        "{message}"
    );
    assert!(!message.contains("пригодных чтений"), "{message}");
    assert_eq!(snapshot(&root.join("dictionaries")), good);
    assert!(manager::verify(&loc, &all()).unwrap().ok);
}

/// У источника частотности корпус задают записи метаданных терминов, а
/// чтений нет вовсе: усечение видно по ним и только по ним.
#[test]
fn update_to_a_shrunken_frequency_corpus_is_refused_by_its_own_measure() {
    let tmp = TempDir::new("shrunk-frequency");
    let root = tmp.path().join("data");
    let (loc, up) = installed(&root);
    let good = snapshot(&root.join("dictionaries"));
    // Одна запись частотности вместо четырёх.
    up.publish(
        "jpdb-v2.2-freq-kana",
        "2024-10-13",
        jpdb_rows(
            "JPDB v2.2 Frequency Kana 2024-10-13",
            &[json!(["学校", "freq", {"value": 616, "displayValue": "616"}])],
        ),
    );
    let update =
        manager::update(&root, &one("jpdb-v2.2-freq-kana"), &up, &Options::default()).unwrap();
    assert_eq!(
        outcome(&update, "jpdb-v2.2-freq-kana"),
        Outcome::Failed,
        "{update:#?}"
    );
    let message = &update.items[0].message;
    assert!(
        message.contains("записей обязательных банков 1 против 4"),
        "{message}"
    );
    assert!(!message.contains("пригодных чтений"), "{message}");
    assert_eq!(snapshot(&root.join("dictionaries")), good);
    assert!(manager::verify(&loc, &all()).unwrap().ok);
}

/// Обычные изменения обновляются: небольшое уменьшение, равноценная замена и
/// рост не считаются аномалией. Граница — ровно половина сохранённого
/// содержания: она допустима, меньше — нет.
#[test]
fn legitimate_decrease_growth_and_replacement_update_normally() {
    let tmp = TempDir::new("legit-changes");
    let up = FakeUpstream::standard();

    // Первый `install` маленького словаря: сравнивать не с чем, поэтому
    // запрета из-за отсутствия базовой версии быть не может.
    let fresh = tmp.path().join("fresh");
    up.publish("jitendex", "T1", lexicon_archive("T1", &LEXICON_PAIRS[..2]));
    let report =
        manager::install(&fresh, &one("jitendex"), &up, None, &Options::default()).unwrap();
    assert_eq!(
        outcome(&report, "jitendex"),
        Outcome::Installed,
        "{report:#?}"
    );

    // Обновления исправного словаря.
    let root = tmp.path().join("data");
    up.publish("jitendex", "T1", lexicon_archive("T1", &LEXICON_PAIRS[..8]));
    let report = manager::install(&root, &one("jitendex"), &up, None, &Options::default()).unwrap();
    assert_eq!(
        outcome(&report, "jitendex"),
        Outcome::Installed,
        "{report:#?}"
    );
    for (release, pairs) in [
        // Ровно половина сохранена — это ещё допустимая правка, не усечение.
        ("T2", &LEXICON_PAIRS[..4]),
        // Равноценная замена: статей столько же, состав другой.
        ("T3", &LEXICON_PAIRS[5..9]),
        // Рост корпуса.
        ("T4", &LEXICON_PAIRS[..]),
    ] {
        up.publish("jitendex", release, lexicon_archive(release, pairs));
        let update = manager::update(&root, &one("jitendex"), &up, &Options::default()).unwrap();
        assert_eq!(
            outcome(&update, "jitendex"),
            Outcome::Updated,
            "{release}: {update:#?}"
        );
    }
    assert!(manager::verify(&location(&root), &all()).unwrap().ok);
    assert_eq!(lookup(&root, "学校").status, Status::SingleReading);
}

/// Как испорчена прежняя версия: устаревшая схема корпуса или повреждённый
/// индекс. Обе делают её непригодной как эталон сравнения.
#[derive(Clone, Copy)]
enum BaselineDamage {
    OutdatedSchema,
    CorruptIndex,
}

/// Неподходящая прежняя версия не становится эталоном: `update` перестраивает
/// её из опубликованного артефакта и не сравнивает содержание с недостоверными
/// счётчиками. Иначе сравнение с ними маскировало бы настоящую причину —
/// необходимость перестроения индекса. Поведение определено явно: кандидат,
/// который против исправной версии был бы отвергнут, здесь принимается.
#[test]
fn damaged_previous_version_is_not_a_baseline_for_comparison() {
    for damage in [BaselineDamage::OutdatedSchema, BaselineDamage::CorruptIndex] {
        let tmp = TempDir::new("baseline-damage");
        let root = tmp.path().join("data");
        let (loc, up) = installed(&root);
        let dir = version_dir(&root, "jitendex");
        match damage {
            BaselineDamage::OutdatedSchema => {
                let store = Store::open_existing(&root).unwrap().unwrap();
                let version = version_name(&root, "jitendex");
                let mut manifest = store.read_manifest("jitendex", &version).unwrap();
                manifest.corpus.schema -= 1;
                store::write_atomic(
                    &dir.join(store::MANIFEST_FILE),
                    &serde_json::to_vec_pretty(&manifest).unwrap(),
                )
                .unwrap();
            }
            BaselineDamage::CorruptIndex => drop_table(&dir.join(store::INDEX_FILE), "readings"),
        }
        // Прежняя версия действительно непригодна — это и есть причина, по
        // которой она не может служить мерой полноты.
        assert!(!manager::verify(&loc, &one("jitendex")).unwrap().ok);

        up.publish("jitendex", "T2", lexicon_archive("T2", &LEXICON_PAIRS[..2]));
        let update = manager::update(&root, &one("jitendex"), &up, &Options::default()).unwrap();
        assert_eq!(
            outcome(&update, "jitendex"),
            Outcome::Updated,
            "{update:#?}"
        );
        assert_eq!(code(&update, "jitendex"), None, "{update:#?}");
        assert!(manager::verify(&loc, &all()).unwrap().ok);
        assert_eq!(lookup(&root, "学校").status, Status::SingleReading);
    }
}

/// Версия с пустым корпусом, установленная до появления проверки, остаётся
/// активной, но `status` и `verify` обязаны её обнаружить: иначе словарь
/// без единой записи навсегда останется «исправным».
#[test]
fn installed_empty_corpus_is_reported_by_status_and_verify() {
    let tmp = TempDir::new("empty-installed");
    let root = tmp.path().join("data");
    let (loc, _up) = installed(&root);

    // Состояние, которое прежнее поведение считало нормальным: архив с
    // пустым обязательным банком и построенный из него пустой корпус.
    let stats = replace_active_version(
        &root,
        "jmnedict",
        empty_dictionary(
            "JMnedict [2026-01-01]",
            "JMnedict.2026-01-01",
            "term_bank_1.json",
        ),
    );
    assert_eq!(stats.terms, 0);

    for report in [
        manager::status(&loc).unwrap(),
        manager::verify(&loc, &all()).unwrap(),
    ] {
        assert!(!report.ok, "пустой словарь не должен считаться исправным");
        let item = report
            .dictionaries
            .iter()
            .find(|d| d.id == "jmnedict")
            .unwrap();
        assert_eq!(item.state, State::Corrupt);
        assert!(
            item.problems.iter().any(|p| p
                .message
                .contains("нет ни одной записи обязательного банка")),
            "{:#?}",
            item.problems
        );
        // Остальные словари набора не затронуты.
        assert!(
            report
                .dictionaries
                .iter()
                .filter(|d| d.id != "jmnedict")
                .all(|d| d.state == State::Ok)
        );
    }

    // Пустой корпус не проходит и глубокую проверку `update`: он не считается
    // актуальным, поэтому рабочая версия с источника публикации возвращается.
    let up = FakeUpstream::standard();
    let update = manager::update(&root, &one("jmnedict"), &up, &Options::default()).unwrap();
    assert_eq!(
        outcome(&update, "jmnedict"),
        Outcome::Updated,
        "{update:#?}"
    );
    assert!(manager::verify(&loc, &all()).unwrap().ok);
    // 田中の чтения даёт только JMnedict: поиск снова отвечает по данным словаря.
    assert_eq!(lookup(&root, "田中").status, Status::ProperNameOnly);
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
    // Тот же архив опубликован источником публикации: проверка распознаёт его по сумме.
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

/// `install` не заменяет установленный словарь с читаемым манифестом, даже
/// когда индекс повреждён; запись с нечитаемым манифестом установленной не
/// считается и ставится заново — из локального ZIP без сети и `update`
/// с источника публикации.
#[test]
fn install_replaces_only_unreadable_manifest_and_keeps_damaged_index() {
    let tmp = TempDir::new("install-repair");
    let root = tmp.path().join("data");
    let (loc, up) = installed(&root);
    let zip = tmp.path().join("Jitendex.zip");
    std::fs::write(&zip, jitendex("Jitendex.test", &[])).unwrap();

    // Повреждённый индекс: манифест читается, установка версию не заменяет.
    let index = version_dir(&root, "jitendex").join(store::INDEX_FILE);
    let mut bytes = std::fs::read(&index).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xFF;
    std::fs::write(&index, &bytes).unwrap();
    assert!(!manager::verify(&loc, &all()).unwrap().ok);
    let damaged = version_dir(&root, "jitendex");
    let report = manager::install(
        &root,
        &one("jitendex"),
        &up,
        Some(&zip),
        &Options::default(),
    )
    .unwrap();
    assert_eq!(outcome(&report, "jitendex"), Outcome::AlreadyInstalled);
    assert!(report.items[0].installed.is_some());
    assert_eq!(
        version_dir(&root, "jitendex"),
        damaged,
        "активная версия не заменена"
    );
    assert!(
        !manager::verify(&loc, &all()).unwrap().ok,
        "повреждение осталось: восстановление — update или remove"
    );

    // Нечитаемый манифест: запись не считается установленной, локальный ZIP её ставит.
    std::fs::write(damaged.join(store::MANIFEST_FILE), b"{").unwrap();
    let report = manager::install(
        &root,
        &one("jitendex"),
        &up,
        Some(&zip),
        &Options::default(),
    )
    .unwrap();
    assert_eq!(outcome(&report, "jitendex"), Outcome::Installed);
    assert_eq!(
        report.items[0].installed.as_ref().unwrap().provider,
        "local_file"
    );
    assert!(manager::verify(&loc, &all()).unwrap().ok);
    assert_eq!(lookup(&root, "学校").status, Status::SingleReading);

    // То же состояние `update` переустанавливает с источника публикации.
    std::fs::write(
        version_dir(&root, "jitendex").join(store::MANIFEST_FILE),
        b"{",
    )
    .unwrap();
    let update = manager::update(&root, &one("jitendex"), &up, &Options::default()).unwrap();
    assert_eq!(
        outcome(&update, "jitendex"),
        Outcome::Updated,
        "{update:#?}"
    );
    assert!(manager::verify(&loc, &all()).unwrap().ok);
    assert_eq!(lookup(&root, "学校").status, Status::SingleReading);
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
    let report = manager::install(&fresh, &all(), &up, None, &Options::default()).unwrap();
    assert!(report.ok, "{report:#?}");
    assert!(
        fresh.join(store::ACTIVE_LOCK).exists(),
        "замок свежей инициализации"
    );
    std::fs::remove_file(fresh.join(store::ACTIVE_LOCK)).unwrap();
    let fresh_before = listing(&fresh);
    assert!(manager::status(&location(&fresh)).unwrap().ok);
    assert_eq!(lookup(&fresh, "学校").status, Status::SingleReading);
    assert_eq!(listing(&fresh), fresh_before, "чтение не создаёт файлов");
    assert!(!fresh.join(store::ACTIVE_LOCK).exists());

    // `remove` без каталога ничего не создаёт.
    let absent = tmp.path().join("absent");
    let report = manager::remove(&absent, &all()).unwrap();
    assert!(
        report
            .items
            .iter()
            .all(|i| i.outcome == Outcome::NotInstalled)
    );
    assert!(!absent.exists(), "remove без каталога ничего не создаёт");
}

/// Целая частотность, записанная как `616.0`, сохраняется числом — и в объекте
/// `{value, displayValue}`, и прямой записью.
#[test]
fn integral_frequency_written_as_float_keeps_the_rank() {
    let tmp = TempDir::new("float-frequency");
    let root = tmp.path().join("data");
    let up = FakeUpstream::standard();
    let report = manager::install(&root, &all(), &up, None, &Options::default()).unwrap();
    assert!(report.ok, "{report:#?}");

    up.publish(
        "jpdb-v2.2-freq-kana",
        "2026-02-02",
        jpdb_rows(
            "JPDB v2.2 Frequency Kana 2026-02-02",
            &[
                // Объект частотности с целым, записанным как `616.0`.
                json!(["学校", "freq", {"reading": "がっこう", "frequency": {"value": 616.0, "displayValue": "616"}}]),
                // Прямая числовая запись: относится к написанию запроса.
                json!(["がっこう", "freq", 5.0]),
            ],
        ),
    );
    let update =
        manager::update(&root, &one("jpdb-v2.2-freq-kana"), &up, &Options::default()).unwrap();
    assert!(update.ok, "{update:#?}");

    let ranks = |surface: &str| {
        let r = lookup(&root, surface);
        let mut ranks: Vec<Option<i64>> = r
            .readings
            .iter()
            .flat_map(|g| g.frequency.iter())
            .map(|f| f.rank)
            .collect();
        ranks.sort();
        ranks
    };
    // Объект частотности: ранг сохранён, а не потерян как `null`.
    assert_eq!(ranks("学校"), vec![Some(616)]);
    // Прямая запись: `5.0` даёт ранг 5, а не `null`.
    assert_eq!(ranks("がっこう"), vec![Some(5)]);
}

/// Повторяющиеся имена записей отвергаются, даже когда читатель ZIP объединяет
/// их во внутренней карте: проверка идёт по числу записей центрального каталога.
#[test]
fn duplicate_entry_names_are_rejected() {
    let tmp = TempDir::new("duplicate-names");
    let root = tmp.path().join("data");
    let up = FakeUpstream::standard();
    let zip = tmp.path().join("Jitendex.zip");
    std::fs::write(
        &zip,
        duplicate_central_record(&jitendex("dup", &[]), "index.json"),
    )
    .unwrap();

    let report = manager::install(
        &root,
        &one("jitendex"),
        &up,
        Some(&zip),
        &Options::default(),
    )
    .unwrap();
    assert_eq!(outcome(&report, "jitendex"), Outcome::Failed);
    assert_eq!(
        code(&report, "jitendex"),
        Some(Code::UnsafeArchive),
        "{report:#?}"
    );
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

/// Оценка статьи Yomitan (`score`, пятый элемент строки банка) объявлена
/// схемой как `number`: дробные значения сохраняются без усечения до целого.
/// Иначе `0.9`, `0.899999999` и `0.1` стали бы одним нулём, и порядок чтений
/// определяла бы нормализованная форма, а не оценка словаря.
#[test]
fn fractional_term_scores_keep_precision_and_reading_order() {
    let tmp = TempDir::new("fractional-score");
    let root = tmp.path().join("data");
    let up = FakeUpstream::standard();
    let gloss = |text: &str| json!([{"type": "structured-content", "content": text}]);
    up.publish(
        "jitendex",
        "T1",
        jitendex(
            "T1",
            &[
                // Порядок чтений по нормализованной форме — かさ, さか, たな,
                // なさ: он и был бы выбран, потеряй оценки дробную часть.
                json!(["試標", "さか", "", "", 0.9, gloss("synthetic"), 500, ""]),
                json!([
                    "試標",
                    "まな",
                    "",
                    "",
                    0.899999999,
                    gloss("synthetic"),
                    501,
                    ""
                ]),
                json!(["試標", "かさ", "", "", 0.1, gloss("synthetic"), 502, ""]),
                json!(["試標", "たな", "", "", -0.5, gloss("synthetic"), 503, ""]),
                json!(["試標", "なさ", "", "", -2, gloss("synthetic"), 504, ""]),
                // Целое значение, записанное как `3.0`: машинный ответ
                // остаётся целым числом.
                json!(["試標", "はな", "", "", 3.0, gloss("synthetic"), 505, ""]),
            ],
        ),
    );
    let report = manager::install(&root, &all(), &up, None, &Options::default()).unwrap();
    assert!(report.ok, "{report:#?}");

    let r = lookup(&root, "試標");
    assert_eq!(r.status, Status::MultipleReadings);
    // Порядок показа групп: оценка статьи по убыванию, а не нормализованная
    // форма чтения (она дала бы かさ, さか, たな, なさ, はな, まな).
    let order: Vec<&str> = r
        .readings
        .iter()
        .filter(|g| g.class == EvidenceClass::LexicalEntry)
        .map(|g| g.normalized.as_str())
        .collect();
    assert_eq!(order, vec!["はな", "さか", "まな", "かさ", "たな", "なさ"]);
    // `distinct_readings` — множество различных чтений без порядка показа.
    assert_eq!(
        r.distinct_readings,
        vec!["かさ", "さか", "たな", "なさ", "はな", "まな"]
    );

    let group = |reading: &str| {
        r.readings
            .iter()
            .find(|g| g.normalized == reading)
            .unwrap_or_else(|| panic!("нет группы {reading}"))
    };
    let score = |reading: &str| group(reading).evidence[0].entry.as_ref().unwrap().score;
    assert_eq!(score("はな"), Some(3.0));
    assert_eq!(score("さか"), Some(0.9));
    assert_eq!(score("まな"), Some(0.899_999_999));
    assert_eq!(score("かさ"), Some(0.1));
    assert_eq!(score("たな"), Some(-0.5));
    assert_eq!(score("なさ"), Some(-2.0));

    // Машинный ответ: дробные оценки остаются дробными, целые — целыми.
    let json = serde_json::to_value(&r).unwrap();
    let json_score = |reading: &str| {
        json["readings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["normalized"] == reading)
            .unwrap()["evidence"][0]["entry"]["score"]
            .clone()
    };
    assert_eq!(json_score("はな"), json!(3));
    assert_eq!(json_score("さか"), json!(0.9));
    assert_eq!(json_score("まな"), json!(0.899_999_999));
    assert_eq!(json_score("かさ"), json!(0.1));
    assert_eq!(json_score("たな"), json!(-0.5));
    assert_eq!(json_score("なさ"), json!(-2));
    assert_eq!(
        json_score("はな").as_f64().unwrap(),
        3.0,
        "целое значение не потеряло величину"
    );
    assert!(
        json_score("はな").is_i64(),
        "целая оценка не стала 3.0: {}",
        json_score("はな")
    );

    // Контроль: обычные целые оценки не изменились ни значением, ни формой.
    for (surface, reading, expected) in [("学校", "がっこう", 200), ("生", "なま", 200)] {
        let control = lookup(&root, surface);
        let value = serde_json::to_value(&control).unwrap();
        let found = value["readings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["normalized"] == reading)
            .unwrap()["evidence"][0]["entry"]["score"]
            .clone();
        assert_eq!(found, json!(expected), "{surface}: {found}");
        assert!(found.is_i64(), "{surface}: целая оценка стала {found}");
    }

    assert!(manager::verify(&location(&root), &all()).unwrap().ok);
}

/// Нечисловая оценка статьи — не «ноль» и не молчаливое усечение:
/// обновление отклоняется целиком, прежняя версия остаётся активной.
#[test]
fn non_numeric_term_score_is_rejected_without_touching_active_version() {
    let tmp = TempDir::new("bad-score");
    let root = tmp.path().join("data");
    let up = FakeUpstream::standard();
    let report = manager::install(&root, &all(), &up, None, &Options::default()).unwrap();
    assert!(report.ok, "{report:#?}");

    let gloss = json!([{"type": "structured-content", "content": "synthetic"}]);
    for (release, bad) in [
        ("T2", json!("0.9")),
        ("T3", json!(null)),
        ("T4", json!(true)),
        ("T5", json!([])),
    ] {
        up.publish(
            "jitendex",
            release,
            jitendex(
                release,
                &[json!(["試標", "さか", "", "", bad, gloss, 600, ""])],
            ),
        );
        let update = manager::update(&root, &all(), &up, &Options::default()).unwrap();
        assert!(!update.ok, "{release}: {update:#?}");
        assert_eq!(outcome(&update, "jitendex"), Outcome::Failed, "{release}");
        assert_eq!(
            code(&update, "jitendex"),
            Some(Code::InvalidFormat),
            "{release}"
        );
        // Активной остаётся прежняя версия, поиск работает.
        assert_eq!(
            lookup(&root, "学校").status,
            Status::SingleReading,
            "{release}"
        );
    }
}

/// `sequence` статьи — целое или `null`: строка, логическое значение и дробное
/// число отклоняются целиком, а целое, записанное как `1.0`, остаётся целым.
#[test]
fn invalid_term_sequence_is_rejected_without_touching_active_version() {
    let tmp = TempDir::new("bad-sequence");
    let root = tmp.path().join("data");
    let up = FakeUpstream::standard();
    let report = manager::install(&root, &all(), &up, None, &Options::default()).unwrap();
    assert!(report.ok, "{report:#?}");

    let gloss = json!([{"type": "structured-content", "content": "synthetic"}]);
    for (release, bad) in [
        ("T2", json!("1206730")),
        ("T3", json!(true)),
        ("T4", json!(1.5)),
    ] {
        up.publish(
            "jitendex",
            release,
            jitendex(
                release,
                &[json!(["試標", "さか", "", "", 200, gloss, bad, ""])],
            ),
        );
        let update = manager::update(&root, &all(), &up, &Options::default()).unwrap();
        assert!(!update.ok, "{release}: {update:#?}");
        assert_eq!(outcome(&update, "jitendex"), Outcome::Failed, "{release}");
        assert_eq!(
            code(&update, "jitendex"),
            Some(Code::InvalidFormat),
            "{release}"
        );
        // Активной остаётся прежняя версия, поиск работает.
        assert_eq!(
            lookup(&root, "学校").status,
            Status::SingleReading,
            "{release}"
        );
    }

    // `null` и целое, записанное как `1.0`, остаются допустимыми.
    up.publish(
        "jitendex",
        "T5",
        jitendex(
            "T5",
            &[
                json!(["試標", "さか", "", "", 200, gloss, 1.0, ""]),
                json!(["試標", "たな", "", "", 200, gloss, null, ""]),
            ],
        ),
    );
    let update = manager::update(&root, &all(), &up, &Options::default()).unwrap();
    assert!(update.ok, "{update:#?}");

    let r = lookup(&root, "試標");
    let sequence = |reading: &str| {
        r.readings
            .iter()
            .find(|g| g.normalized == reading)
            .unwrap_or_else(|| panic!("нет группы {reading}"))
            .evidence[0]
            .entry
            .as_ref()
            .unwrap()
            .sequence
    };
    assert_eq!(sequence("さか"), Some(1));
    assert_eq!(sequence("たな"), None);
}

/// Отказ одного индекса во время поиска не отменяет поиск по исправным словарям:
/// источник помечается недоступным, его частичные свидетельства в ответ не
/// попадают, а `complete` становится false.
#[test]
fn index_failure_during_lookup_marks_source_unavailable() {
    let tmp = TempDir::new("index-failure");
    let root = tmp.path().join("data");
    installed(&root);

    // У jitendex удалена таблица указателей: индекс открывается, а чтение
    // указателей падает уже во время поиска.
    let index = version_dir(&root, "jitendex").join(store::INDEX_FILE);
    drop_table(&index, "redirects");

    let r = lookup(&root, "学校");
    assert!(
        r.readings.is_empty(),
        "частичные свидетельства отказавшего словаря не попадают в ответ: {r:#?}"
    );
    assert!(!r.complete);
    assert_eq!(r.missing_sources, vec!["jitendex"]);
    assert_eq!(r.status, Status::SourceMissing);
    let jitendex = r
        .sources
        .iter()
        .find(|s| s.id == "jitendex")
        .expect("нет отчёта об источнике");
    assert_eq!(jitendex.state, SourceState::Unavailable);
    assert_eq!(jitendex.code, Some(Code::IndexCorrupt));

    // Исправные словари продолжают отвечать, отказ виден и в их ответе.
    let r = lookup(&root, "田中");
    assert!(
        r.readings
            .iter()
            .any(|g| g.class == EvidenceClass::ProperName),
        "{r:#?}"
    );
    assert!(!r.complete);
    assert_eq!(r.missing_sources, vec!["jitendex"]);
}

/// `sort_order` и `score` банка тегов схема Yomitan объявляет как `number`:
/// дробная помета сохраняется числом без усечения и насыщения, а нечисловое
/// значение отклоняет обновление целиком, не трогая активную версию.
#[test]
fn fractional_tag_numbers_are_kept_and_non_numeric_ones_rejected() {
    let tmp = TempDir::new("tag-numbers");
    let root = tmp.path().join("data");
    let up = FakeUpstream::standard();
    let report = manager::install(&root, &all(), &up, None, &Options::default()).unwrap();
    assert!(report.ok, "{report:#?}");

    let gloss = json!([{"type": "structured-content", "content": "synthetic"}]);
    let bank = |order: Value, score: Value| {
        json!([
            ["★", "popular", order, "high priority entry", score],
            [
                "priority\u{a0}form",
                "frequent",
                1,
                "high priority spelling",
                1
            ],
            [
                "old\u{a0}kanji\u{a0}form",
                "archaism",
                0,
                "old character form",
                1
            ]
        ])
    };
    let publish = |release: &str, order: Value, score: Value| {
        up.publish(
            "jitendex",
            release,
            jitendex_with_tags(
                release,
                &[json!(["試標", "さか", "", "", 200, gloss, 600, ""])],
                bank(order, score),
            ),
        );
    };

    // Дробная помета допустима схемой и сохраняется числом.
    publish("T2", json!(1.5), json!(-0.5));
    let update = manager::update(&root, &all(), &up, &Options::default()).unwrap();
    assert!(update.ok, "{update:#?}");
    assert_eq!(tag_numbers(&root, "★"), (1.5, -0.5));

    // Большое конечное число не насыщается границей `i64`.
    publish("T3", json!(1e30), json!(2.0));
    let update = manager::update(&root, &all(), &up, &Options::default()).unwrap();
    assert!(update.ok, "{update:#?}");
    assert!(manager::verify(&location(&root), &all()).unwrap().ok);
    assert_eq!(tag_numbers(&root, "★"), (1e30, 2.0));

    // Нечисловое значение: обновление отклоняется целиком, прежняя версия цела.
    for (release, bad) in [("T4", json!("2")), ("T5", json!(true))] {
        publish(release, bad, json!(2));
        let update = manager::update(&root, &all(), &up, &Options::default()).unwrap();
        assert!(!update.ok, "{release}: {update:#?}");
        assert_eq!(outcome(&update, "jitendex"), Outcome::Failed, "{release}");
        assert_eq!(
            code(&update, "jitendex"),
            Some(Code::InvalidFormat),
            "{release}"
        );
        assert_eq!(
            lookup(&root, "学校").status,
            Status::SingleReading,
            "{release}"
        );
        assert_eq!(tag_numbers(&root, "★"), (1e30, 2.0), "{release}");
    }
}

/// Числовые поля пометы в установленном индексе: `sort_order` и `score`.
fn tag_numbers(root: &Path, name: &str) -> (f64, f64) {
    let index = version_dir(root, "jitendex").join(store::INDEX_FILE);
    let conn = rusqlite::Connection::open(index).unwrap();
    conn.query_row(
        "SELECT sort_order, score FROM tags WHERE name = ?1",
        [name],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .unwrap()
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
