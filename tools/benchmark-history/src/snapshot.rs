//! Снимок последнего подтверждённого измерения на странице модуля `tools/<модуль>/performance.md`.
//!
//! Страница модуля хранит по одному блоку на пару «сценарий × профиль» в
//! явно названном эталонном окружении. Блок генерируется из выбранного
//! проверенного результата на трёх языках документации одновременно и
//! заменяет только блок той же пары. Другое окружение или оборудование,
//! другой или неизвестный объём нагрузки, недоказанное тождество нагрузки,
//! неполная серия, изменённая рабочая копия или более старый результат
//! прежний снимок молча не заменяют. В постоянной документации нет коммитов,
//! идентификаторов записей, идентичностей входов и путей: тождество нагрузки
//! доказывается сверкой с результатом прежнего снимка в локальной истории —
//! другим результатом, а не публикуемым, который даёт в точности тот же блок.

use crate::format;
use benchmark_contract::result::{
    Aggregate, Availability, BenchmarkResult, Fact, Identity, OutcomeStatus, Quantity, RunStatus,
};
use benchmark_contract::scenario::{InputRole, Scenario};
use benchmark_contract::vocabulary::BuiltinFact;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

const BEGIN: &str = "<!-- benchmark-snapshot: ";
const END: &str = "<!-- /benchmark-snapshot -->";
pub const ANCHOR: &str = "<!-- benchmark-snapshot:anchor -->";
const I18N_MARK: &str = "<!-- i18n-source-git-blob: ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Ru,
    En,
    ZhCn,
}

impl Lang {
    pub const ALL: [Lang; 3] = [Lang::Ru, Lang::En, Lang::ZhCn];

    fn page(self, ru: &Path) -> PathBuf {
        let stem = ru
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        match self {
            Lang::Ru => ru.to_path_buf(),
            Lang::En => ru.with_file_name(format!("{stem}.en.md")),
            Lang::ZhCn => ru.with_file_name(format!("{stem}.zh-CN.md")),
        }
    }
}

/// Машинная часть маркера блока: по ней блок находится и сверяется.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    pub scenario: String,
    pub profile: String,
    pub recorded_at: String,
    /// Эталонное окружение снимка: [`REFERENCE_FACTS`] и факты
    /// сопоставимости сценария.
    pub environment: BTreeMap<String, String>,
    /// Объём входов роли `workload` по ID входа: эталонная нагрузка снимка.
    /// Маркер без этого поля читается, но нагрузка такого снимка неизвестна,
    /// и её смену подтверждает только `--replace-workload`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload: Option<BTreeMap<String, BTreeMap<String, Quantity>>>,
    /// Записанные параметры, влияющие на сопоставимость (например, число
    /// писателей): результаты с разными значениями несопоставимы, поэтому
    /// смену параметров снимка, как и смену нагрузки, подтверждает только
    /// `--replace-workload`. Пустой набор в маркер не пишется.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
struct Block {
    start: usize,
    end: usize,
    meta: Meta,
}

/// Блоки страницы по строкам маркеров.
fn blocks(text: &str) -> Result<Vec<Block>, String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut found = Vec::new();
    let mut open: Option<(usize, Meta)> = None;
    for (n, line) in lines.iter().enumerate() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix(BEGIN) {
            if open.is_some() {
                return Err(format!("строка {}: блок снимка открыт повторно", n + 1));
            }
            let json = rest
                .strip_suffix(" -->")
                .ok_or_else(|| format!("строка {}: маркер снимка не закрыт", n + 1))?;
            let meta: Meta = serde_json::from_str(json)
                .map_err(|e| format!("строка {}: маркер снимка не разобран: {e}", n + 1))?;
            open = Some((n, meta));
        } else if line == END {
            let (start, meta) = open
                .take()
                .ok_or_else(|| format!("строка {}: конец снимка без начала", n + 1))?;
            found.push(Block {
                start,
                end: n,
                meta,
            });
        }
    }
    if open.is_some() {
        return Err("блок снимка не закрыт".into());
    }
    Ok(found)
}

/// Аппаратные и платформенные факты, которые входят в идентичность
/// эталонного окружения снимка всегда, даже если сценарий не делает их
/// условием сопоставимости: другая машина — другой процессор, объём памяти,
/// смена WSL2 на Linux без виртуализации — не заменяет публичный эталон молча.
/// Сопоставимость результатов по-прежнему решает только `comparable`.
pub const REFERENCE_FACTS: &[BuiltinFact] = &[
    BuiltinFact::Os,
    BuiltinFact::Arch,
    BuiltinFact::Virtualization,
    BuiltinFact::CpuModel,
    BuiltinFact::LogicalCpus,
    BuiltinFact::MemoryTotalBytes,
];

/// Эталонное окружение — известные значения [`REFERENCE_FACTS`] и фактов
/// сопоставимости сценария. Неизвестный факт совпадающим не считается:
/// эталон без него не определён.
pub fn reference_environment(
    scenario: &Scenario,
    result: &BenchmarkResult,
) -> Result<BTreeMap<String, String>, String> {
    let names: BTreeSet<String> = REFERENCE_FACTS
        .iter()
        .map(|f| f.as_str().to_owned())
        .chain(scenario.environment.comparable.iter().cloned())
        .collect();
    let mut environment = BTreeMap::new();
    for name in names {
        match result.environment.get(&name) {
            Some(Fact::Known { value }) => {
                environment.insert(name, value.clone());
            }
            Some(Fact::Unavailable { reason }) => {
                return Err(format!(
                    "факт окружения {name} неизвестен ({reason}): эталонное окружение снимка не определено"
                ));
            }
            None => {
                return Err(format!(
                    "факт окружения {name} не записан: эталонное окружение снимка не определено; сценарий должен записывать его в environment.record"
                ));
            }
        }
    }
    Ok(environment)
}

/// Эталонная нагрузка — объём входов роли `workload`; идентичность в документацию
/// не попадает.
pub fn reference_workload(
    result: &BenchmarkResult,
) -> BTreeMap<String, BTreeMap<String, Quantity>> {
    result
        .inputs
        .iter()
        .filter(|input| input.role == InputRole::Workload)
        .map(|input| (input.id.clone(), input.size.clone()))
        .collect()
}

/// Идентичности входов роли `workload`. В документацию не попадают: ими
/// тождество нагрузки сверяется только с локальной историей.
pub fn workload_identities(result: &BenchmarkResult) -> BTreeMap<String, Identity> {
    result
        .inputs
        .iter()
        .filter(|input| input.role == InputRole::Workload)
        .map(|input| (input.id.clone(), input.identity.clone()))
        .collect()
}

/// Действующие значения параметров, объявленных влияющими на сопоставимость;
/// отсутствующее значение берётся из `default` сценария.
pub fn comparability_parameters(
    scenario: &Scenario,
    result: &BenchmarkResult,
) -> BTreeMap<String, String> {
    scenario
        .parameters
        .iter()
        .filter(|parameter| parameter.affects_comparability)
        .filter_map(|parameter| {
            result
                .parameters
                .get(&parameter.name)
                .or(parameter.default.as_ref())
                .map(|value| (parameter.name.clone(), value.clone()))
        })
        .collect()
}

/// Машинная часть маркера, которую результат даёт снимку.
fn meta_of(scenario: &Scenario, result: &BenchmarkResult) -> Result<Meta, String> {
    Ok(Meta {
        scenario: result.scenario.id.clone(),
        profile: result.profile.id.clone(),
        recorded_at: result.provenance.recorded_at.clone(),
        environment: reference_environment(scenario, result)?,
        workload: Some(reference_workload(result)),
        parameters: comparability_parameters(scenario, result),
    })
}

/// Отпечаток содержимого результата: по нему публикуемый результат
/// исключается из доказательства происхождения прежнего снимка.
fn fingerprint(result: &BenchmarkResult) -> String {
    let bytes = serde_json::to_vec(result).expect("JSON");
    crate::archive::hex(&Sha256::digest(&bytes))
}

/// Результат локальной истории, который мог дать прежний снимок: блоки,
/// которые он даёт на трёх языках, и идентичности его нагрузки.
#[derive(Debug, Clone)]
pub struct Recorded {
    fingerprint: String,
    /// Маркер результата; `None`, если эталонное окружение не определено:
    /// такой результат не может быть источником опубликованного снимка.
    meta: Option<Meta>,
    /// Блоки в порядке [`Lang::ALL`] без завершающего перевода строки.
    blocks: Vec<String>,
    identities: BTreeMap<String, Identity>,
}

impl Recorded {
    pub fn of(scenario: &Scenario, result: &BenchmarkResult) -> Recorded {
        let meta = meta_of(scenario, result).ok();
        let blocks = meta
            .as_ref()
            .map(|meta| {
                Lang::ALL
                    .iter()
                    .map(|lang| {
                        render(*lang, meta, result)
                            .trim_end_matches('\n')
                            .to_owned()
                    })
                    .collect()
            })
            .unwrap_or_default();
        Recorded {
            fingerprint: fingerprint(result),
            meta,
            blocks,
            identities: workload_identities(result),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Разрешить заменить снимок, сделанный в другом окружении.
    pub replace_environment: bool,
    /// Разрешить заменить снимок, тождество нагрузки которого с новой не
    /// доказано: другой объём, другое содержимое или прежний результат неизвестен.
    pub replace_workload: bool,
    /// Разрешить результат рабочей копии с изменениями (это видно в блоке).
    pub allow_dirty: bool,
    /// Результаты локальной истории той же пары: доказательство тождества
    /// нагрузки прежнего снимка. Пустой список — доказательства нет. Сам
    /// публикуемый результат, если он здесь есть, доказательством не считается.
    pub history: Vec<Recorded>,
}

/// Чем подтверждена эталонная нагрузка обновлённого снимка.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkloadBasis {
    /// Снимка этой пары на странице не было.
    New,
    /// Сценарий не объявляет входов роли `workload`.
    NoWorkload,
    /// В локальной истории есть другой результат, дающий в точности блок
    /// прежнего снимка, и идентичности нагрузки всех таких результатов
    /// совпадают с новой.
    ProvenByHistory,
    /// Тождество не доказано или нагрузка другая; замену явно подтвердил
    /// `--replace-workload`.
    ConfirmedReplacement,
}

impl WorkloadBasis {
    /// Порядок, в котором основание представляет план нескольких страниц:
    /// явное подтверждение важнее доказательства, замена — добавления.
    fn weight(self) -> u8 {
        match self {
            WorkloadBasis::New => 0,
            WorkloadBasis::NoWorkload => 1,
            WorkloadBasis::ProvenByHistory => 2,
            WorkloadBasis::ConfirmedReplacement => 3,
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            WorkloadBasis::New => "снимка этой пары не было, сравнивать нагрузку не с чем",
            WorkloadBasis::NoWorkload => "сценарий не объявляет входов нагрузки",
            WorkloadBasis::ProvenByHistory => {
                "тождество нагрузки доказано: другой результат локальной истории даёт в точности блок прежнего снимка, и идентичности входов совпали"
            }
            WorkloadBasis::ConfirmedReplacement => {
                "тождество нагрузки не доказано; смену эталонной нагрузки явно подтвердил --replace-workload"
            }
        }
    }
}

/// Что изменится на страницах.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub action: Action,
    pub workload: WorkloadBasis,
    pub pages: Vec<PageChange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    Added,
    Replaced,
    Unchanged,
}

#[derive(Debug, Clone, Serialize)]
pub struct PageChange {
    pub path: PathBuf,
    #[serde(skip)]
    pub before: String,
    #[serde(skip)]
    pub after: String,
    pub block: String,
}

/// Подготовить замену блока на всех трёх языках, ничего не записывая.
pub fn plan(
    page: &Path,
    scenario: &Scenario,
    result: &BenchmarkResult,
    options: &Options,
) -> Result<Plan, String> {
    if result.outcome.status != OutcomeStatus::Success {
        return Err(format!(
            "итог {:?}: снимок публикуется только из успешной серии",
            result.outcome.status
        )
        .to_lowercase());
    }
    if result.scenario.id != scenario.id || result.scenario.revision != scenario.revision {
        return Err(format!(
            "результат записан для {} r{}, а каталог объявляет {} r{}: устаревший результат не публикуется",
            result.scenario.id, result.scenario.revision, scenario.id, scenario.revision
        ));
    }
    let dirty = result.provenance.code.dirty;
    if dirty != Some(false) && !options.allow_dirty {
        return Err(
            "результат измерен в рабочей копии с изменениями или без записанного состояния: публикуйте измерение закоммиченного кода либо явно укажите --allow-dirty".into(),
        );
    }
    let meta = meta_of(scenario, result)?;
    let new = Published {
        meta: &meta,
        identities: workload_identities(result),
        fingerprint: fingerprint(result),
    };
    let mut pages = Vec::new();
    let mut action = Action::Unchanged;
    let mut workload: Option<WorkloadBasis> = None;
    for (n, lang) in Lang::ALL.into_iter().enumerate() {
        let path = lang.page(page);
        let before = fs::read_to_string(&path)
            .map_err(|e| format!("{}: страница не читается: {e}", path.display()))?;
        let block = render(lang, &meta, result);
        let (after, this, basis) = place(&before, n, &new, &block, options)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        // Замена на одной из страниц важнее добавления на другой, а самое
        // слабое основание замены не скрывается более сильным.
        if this == Action::Replaced || action == Action::Unchanged {
            action = this;
        }
        if workload.is_none_or(|w| basis.weight() > w.weight()) {
            workload = Some(basis);
        }
        pages.push(PageChange {
            path,
            before,
            after,
            block,
        });
    }
    Ok(Plan {
        action,
        workload: workload.expect("три страницы"),
        pages,
    })
}

/// Публикуемый результат: его маркер, идентичности нагрузки и отпечаток.
struct Published<'a> {
    meta: &'a Meta,
    identities: BTreeMap<String, Identity>,
    fingerprint: String,
}

fn place(
    text: &str,
    lang: usize,
    new: &Published,
    block: &str,
    options: &Options,
) -> Result<(String, Action, WorkloadBasis), String> {
    let meta = new.meta;
    let existing = blocks(text)?;
    let lines: Vec<&str> = text.split('\n').collect();
    let same = existing
        .iter()
        .find(|b| b.meta.scenario == meta.scenario && b.meta.profile == meta.profile);
    let (start, end, action, basis) = match same {
        Some(old) => {
            if old.meta.environment != meta.environment && !options.replace_environment {
                let unknown: Vec<&str> = meta
                    .environment
                    .keys()
                    .filter(|k| !old.meta.environment.contains_key(*k))
                    .map(String::as_str)
                    .collect();
                let unknown = if unknown.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; в прежнем маркере не записаны {}, и совпадение не доказано",
                        unknown.join(", ")
                    )
                };
                return Err(format!(
                    "снимок {} · {} получен в другом эталонном окружении ({}); новое окружение ({}) его молча не заменяет{unknown} — подтвердите смену эталона флагом --replace-environment",
                    meta.scenario,
                    meta.profile,
                    describe(&old.meta.environment),
                    describe(&meta.environment)
                ));
            }
            let old_key = crate::index::time_key(&old.meta.recorded_at);
            let new_key = crate::index::time_key(&meta.recorded_at);
            if old_key > new_key {
                return Err(format!(
                    "на странице более новое подтверждённое измерение ({}) чем выбранное ({})",
                    old.meta.recorded_at, meta.recorded_at
                ));
            }
            let current = lines[old.start..=old.end]
                .iter()
                .map(|l| l.trim_end_matches('\r'))
                .collect::<Vec<_>>()
                .join("\n");
            // Совпавший текст блока тождества нагрузки не доказывает: её
            // идентичностей в блоке нет, поэтому проверка та же, что при замене.
            let basis = workload_basis(&old.meta, &current, lang, new, options)?;
            if current == block.trim_end_matches('\n') {
                return Ok((text.to_owned(), Action::Unchanged, basis));
            }
            (old.start, old.end + 1, Action::Replaced, basis)
        }
        None => {
            let anchor = lines
                .iter()
                .position(|l| l.trim_end_matches('\r') == ANCHOR)
                .ok_or_else(|| {
                    format!("на странице нет снимка этой пары и строки {ANCHOR}, перед которой добавляются новые снимки")
                })?;
            (anchor, anchor, Action::Added, WorkloadBasis::New)
        }
    };
    let mut out: Vec<String> = lines[..start].iter().map(|s| (*s).to_owned()).collect();
    // Вставленные строки получают окончания строк самой страницы.
    let eol = if text.contains("\r\n") { "\r" } else { "" };
    out.extend(
        block
            .trim_end_matches('\n')
            .split('\n')
            .map(|l| format!("{l}{eol}")),
    );
    if action == Action::Added {
        out.push(eol.to_owned());
    }
    out.extend(lines[end..].iter().map(|s| (*s).to_owned()));
    Ok((out.join("\n"), action, basis))
}

/// Тождество нагрузки прежнего снимка и нового результата. Совпадение объёма
/// его не доказывает: два разных входа могут иметь одинаковые размеры. Сами
/// идентичности на странице не хранятся, поэтому доказательство — результат
/// локальной истории, который даёт в точности блок прежнего снимка (маркер,
/// момент записи, окружение, метрики), и идентичности его нагрузки. Публикуемый
/// результат такого доказательства не даёт: иначе новый результат того же
/// объёма, записанный в ту же секунду, подтвердил бы сам себя. Несколько
/// источников с разной нагрузкой — происхождение неоднозначно. Без
/// доказательства замена требует явного `--replace-workload`.
fn workload_basis(
    old: &Meta,
    current: &str,
    lang: usize,
    new: &Published,
    options: &Options,
) -> Result<WorkloadBasis, String> {
    let pair = format!("{} · {}", new.meta.scenario, new.meta.profile);
    let confirm =
        "подтвердите смену или неподтверждённость эталонной нагрузки флагом --replace-workload";
    let refuse = |reason: String| {
        if options.replace_workload {
            Ok(WorkloadBasis::ConfirmedReplacement)
        } else {
            Err(reason)
        }
    };
    let Some(old_workload) = &old.workload else {
        return refuse(format!(
            "в маркере снимка {pair} эталонная нагрузка не записана: совпадение с новой нагрузкой не доказано — {confirm}"
        ));
    };
    if old.parameters != new.meta.parameters {
        return refuse(format!(
            "снимок {pair} получен с другими параметрами, влияющими на сопоставимость ({}); новые параметры ({}) его молча не заменяют — {confirm}",
            describe(&old.parameters),
            describe(&new.meta.parameters)
        ));
    }
    if Some(old_workload) != new.meta.workload.as_ref() {
        return refuse(format!(
            "снимок {pair} получен на нагрузке другого объёма; новая нагрузка его молча не заменяет — {confirm}"
        ));
    }
    if old_workload.is_empty() && new.identities.is_empty() {
        return Ok(WorkloadBasis::NoWorkload);
    }
    let marked: Vec<&Recorded> = options
        .history
        .iter()
        .filter(|r| r.meta.as_ref() == Some(old))
        .collect();
    let gives_block = |r: &&&Recorded| r.blocks.get(lang).map(String::as_str) == Some(current);
    let itself = marked
        .iter()
        .any(|r| r.fingerprint == new.fingerprint && gives_block(&r));
    let others: Vec<&&Recorded> = marked
        .iter()
        .filter(|r| r.fingerprint != new.fingerprint)
        .collect();
    let sources: Vec<&&Recorded> = others.iter().copied().filter(gives_block).collect();
    if sources.is_empty() {
        return refuse(if itself {
            format!(
                "блок снимка {pair} даёт сам публикуемый результат, а другого результата с этим блоком в локальной истории нет: маркер не хранит идентичностей входов, поэтому публикуемый результат не доказывает происхождения прежнего снимка (повторный запуск той же публикации страницы не меняет) — {confirm}"
            )
        } else if !others.is_empty() {
            format!(
                "в локальной истории есть результат с данными маркера снимка {pair} ({}), но блок на странице с ним не совпадает (изменён вручную или сгенерирован другой версией генератора): происхождение прежнего снимка не доказано — {confirm}",
                old.recorded_at
            )
        } else {
            format!(
                "снимок {pair} получен на нагрузке того же объёма, но объём не доказывает тождество входа, а результата прежнего снимка ({}) в локальной истории нет — тождество нагрузки не доказано; восстановите историю этого результата или {confirm}",
                old.recorded_at
            )
        });
    }
    let ambiguous: BTreeSet<&str> = sources
        .iter()
        .flat_map(|r| differing(&r.identities, &sources[0].identities))
        .collect();
    if !ambiguous.is_empty() {
        return refuse(format!(
            "блок снимка {pair} дают несколько результатов локальной истории с разной нагрузкой (входы {}): происхождение прежнего снимка неоднозначно — {confirm}",
            ambiguous.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    let changed = differing(&sources[0].identities, &new.identities);
    if changed.is_empty() {
        return Ok(WorkloadBasis::ProvenByHistory);
    }
    refuse(format!(
        "снимок {pair} получен на нагрузке того же объёма, но с другим содержимым: идентичности входов {} отличаются от результата прежнего снимка в локальной истории — {confirm}",
        changed.into_iter().collect::<Vec<_>>().join(", ")
    ))
}

/// ID входов, идентичности которых различаются.
fn differing<'a>(
    a: &'a BTreeMap<String, Identity>,
    b: &'a BTreeMap<String, Identity>,
) -> BTreeSet<&'a str> {
    a.keys()
        .chain(b.keys())
        .filter(|id| a.get(*id) != b.get(*id))
        .map(String::as_str)
        .collect()
}

fn describe(environment: &BTreeMap<String, String>) -> String {
    environment
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Git blob SHA-1 содержимого: им переводы подтверждают русский оригинал.
pub fn git_blob_sha(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {}\0", bytes.len()).as_bytes());
    hasher.update(bytes);
    crate::archive::hex(&hasher.finalize())
}

/// Шаг записи страниц: точка внедрения отказа в проверках. Номер — позиция
/// страницы в плане (0 — русская).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Запись временной копии новой страницы.
    Prepare(usize),
    /// Замена страницы временной копией.
    Commit(usize),
    /// Возврат уже заменённой страницы к прежнему тексту после отказа.
    Restore(usize),
}

/// Записать страницы. Отметка перевода обновляется, только если перевод был
/// подтверждён для прежнего русского текста: тогда изменение — тот же
/// сгенерированный блок на всех языках, и синхронность сохраняется.
///
/// Гарантия согласованности: пока любая страница не подготовлена, ни одна не
/// меняется; страницы заменяются по одной переименованием в порядке RU, EN,
/// zh-CN, и при отказе замены уже заменённые возвращаются к прежнему тексту.
/// Если не удался и возврат, ошибка называет, какие страницы обновлены, а
/// какие нет. Прерванный процесс между переименованиями оставляет переводы с
/// отметкой i18n прежнего русского текста, и проверка документации это видит.
pub fn apply(plan: &Plan) -> Result<Vec<String>, String> {
    apply_with(plan, &mut |_| Ok(()))
}

/// [`apply`] с внедрением отказа перед каждым шагом записи.
pub fn apply_with(
    plan: &Plan,
    fault: &mut dyn FnMut(Step) -> std::io::Result<()>,
) -> Result<Vec<String>, String> {
    let mut notes = Vec::new();
    let ru = &plan.pages[0];
    let old_sha = git_blob_sha(ru.before.as_bytes());
    let new_sha = git_blob_sha(ru.after.as_bytes());
    let mut changes: Vec<(usize, &PageChange, String)> = Vec::new();
    for (n, page) in plan.pages.iter().enumerate() {
        let mut text = page.after.clone();
        if n > 0 {
            let old_mark = format!("{I18N_MARK}{old_sha} -->");
            if text.contains(&old_mark) {
                text = text.replacen(&old_mark, &format!("{I18N_MARK}{new_sha} -->"), 1);
            } else if text != page.before {
                notes.push(format!(
                    "{}: перевод не был подтверждён для прежнего русского текста, отметка i18n не обновлена",
                    page.path.display()
                ));
            }
        }
        if text != page.before {
            changes.push((n, page, text));
        }
    }
    // Страница, изменённая после подготовки плана, не перезаписывается.
    for (_, page, _) in &changes {
        let current = fs::read_to_string(&page.path).map_err(|e| {
            format!(
                "{}: страница не читается: {e}; страницы не изменены",
                page.path.display()
            )
        })?;
        if current != page.before {
            return Err(format!(
                "{}: страница изменилась после подготовки снимка; страницы не изменены, повторите snapshot",
                page.path.display()
            ));
        }
    }
    let tmp = |page: &PageChange| {
        page.path
            .with_extension(format!("md.{}.snapshot.tmp", std::process::id()))
    };
    let write = |path: &Path, text: &str| -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()
    };
    let mut prepared = Vec::new();
    for (n, page, text) in &changes {
        let path = tmp(page);
        if let Err(e) = fault(Step::Prepare(*n)).and_then(|()| write(&path, text)) {
            let _ = fs::remove_file(&path);
            for done in &prepared {
                let _ = fs::remove_file(done);
            }
            return Err(format!("{}: {e}; страницы не изменены", path.display()));
        }
        prepared.push(path);
    }
    for (k, (n, page, _)) in changes.iter().enumerate() {
        let renamed = fault(Step::Commit(*n)).and_then(|()| fs::rename(&prepared[k], &page.path));
        let Err(error) = renamed else { continue };
        for left in &prepared[k..] {
            let _ = fs::remove_file(left);
        }
        let mut restored = Vec::new();
        let mut stuck = Vec::new();
        for (n, page, _) in changes[..k].iter().rev() {
            let back = tmp(page);
            let undone = fault(Step::Restore(*n))
                .and_then(|()| write(&back, &page.before))
                .and_then(|()| fs::rename(&back, &page.path));
            match undone {
                Ok(()) => restored.push(page.path.display().to_string()),
                Err(e) => {
                    let _ = fs::remove_file(&back);
                    stuck.push(format!("{} ({e})", page.path.display()));
                }
            }
        }
        if stuck.is_empty() {
            return Err(format!(
                "{}: {error}; страницы не изменены{}",
                page.path.display(),
                if restored.is_empty() {
                    String::new()
                } else {
                    format!(" (возвращены к прежнему тексту: {})", restored.join(", "))
                }
            ));
        }
        let untouched: Vec<String> = changes[k..]
            .iter()
            .map(|(_, p, _)| p.path.display().to_string())
            .collect();
        return Err(format!(
            "{}: {error}; страницы рассогласованы: обновлены и не возвращены {}, не обновлены {}; восстановите их из Git и повторите snapshot",
            page.path.display(),
            stuck.join(", "),
            untouched.join(", ")
        ));
    }
    Ok(notes)
}

struct Labels {
    /// Разделитель «подпись: текст» и конец предложения по нормам языка.
    colon: &'static str,
    stop: &'static str,
    heading: &'static str,
    confirmed: &'static str,
    outcome: fn(usize, usize, u32) -> String,
    dirty: &'static str,
    environment: &'static str,
    value: &'static str,
    workload: &'static str,
    parameter: &'static str,
    no_workload: &'static str,
    metric: &'static str,
    scope: &'static str,
    mean: &'static str,
    median: &'static str,
    range: &'static str,
    not_measured: &'static str,
    reference: &'static str,
    /// Слово «ядро» после числа.
    cores: fn(f64) -> &'static str,
}

fn labels(lang: Lang) -> Labels {
    match lang {
        Lang::Ru => Labels {
            colon: ": ",
            stop: ".",
            heading: "профиль",
            confirmed: "Последнее подтверждённое измерение",
            outcome: |ok, all, warmup| {
                format!("итог `success`: успешных прогонов {ok} из {all}, прогревов {warmup}")
            },
            dirty: "измерена рабочая копия с изменениями",
            environment: "Факт окружения",
            value: "Значение",
            workload: "Нагрузка",
            parameter: "параметр",
            no_workload: "Сценарий не объявляет входов.",
            metric: "Метрика",
            scope: "Scope / источник",
            mean: "Среднее ± σ",
            median: "Медиана",
            range: "Мин–макс",
            not_measured: "Не измерено",
            reference: "Эталонное окружение задают аппаратные и платформенные факты и факты сопоставимости сценария; при обновлении снимок из другого окружения или на другом оборудовании прежний молча не заменяет. Значения — наблюдение этого окружения, а не норматив для других систем и не порог CI.",
            cores: format::cores_ru,
        },
        Lang::En => Labels {
            colon: ": ",
            stop: ".",
            heading: "profile",
            confirmed: "Last confirmed measurement",
            outcome: |ok, all, warmup| {
                let unit = if warmup == 1 { "run" } else { "runs" };
                format!("outcome `success`: {ok} of {all} runs successful, {warmup} warmup {unit}")
            },
            dirty: "a working copy with changes was measured",
            environment: "Environment fact",
            value: "Value",
            workload: "Workload",
            parameter: "parameter",
            no_workload: "The scenario declares no inputs.",
            metric: "Metric",
            scope: "Scope / source",
            mean: "Mean ± σ",
            median: "Median",
            range: "Min–max",
            not_measured: "Not measured",
            reference: "The reference environment is defined by hardware and platform facts and by the scenario's comparability facts; on update, a snapshot from another environment or other hardware never silently replaces this one. The values are an observation of this environment, not a norm for other systems and not a CI threshold.",
            cores: |value| if value == 1.0 { "core" } else { "cores" },
        },
        Lang::ZhCn => Labels {
            colon: "：",
            stop: "。",
            heading: "配置",
            confirmed: "最近一次确认的测量",
            outcome: |ok, all, warmup| {
                format!("结果 `success`：{all} 次运行中 {ok} 次成功，预热 {warmup} 次")
            },
            dirty: "测量的是含未提交修改的工作副本",
            environment: "环境事实",
            value: "值",
            workload: "负载",
            parameter: "参数",
            no_workload: "该场景未声明输入。",
            metric: "指标",
            scope: "范围 / 来源",
            mean: "均值 ± σ",
            median: "中位数",
            range: "最小–最大",
            not_measured: "未测量",
            reference: "参考环境由硬件与平台事实以及场景的可比性事实确定；更新时，来自其他环境或其他硬件的快照不会静默替换本快照。这些数值是该环境的观测结果，不是对其他系统的标准，也不是 CI 阈值。",
            cores: |_| "核",
        },
    }
}

fn marker(meta: &Meta) -> String {
    let json = serde_json::to_string(meta).expect("JSON");
    // `--` внутри HTML-комментария недопустимо: экранируется в строке JSON.
    format!("{BEGIN}{} -->", json.replace("--", "-\\u002d"))
}

/// Блок снимка на одном языке.
fn markdown_table_cell(value: &str) -> String {
    value.replace('\\', "\\\\").replace('|', "\\|")
}

/// Обычные значения сохраняют прежний вид. Произвольный текст представляем
/// JSON-строкой в HTML-коде: видны пробелы и управляющие символы, а Markdown
/// не интерпретирует обратные кавычки и разделители таблицы.
fn markdown_parameter(value: &str) -> String {
    if !value.is_empty()
        && value.trim() == value
        && !value
            .chars()
            .any(|c| c == '`' || c == '\\' || c.is_control())
    {
        return format!("`{}`", markdown_table_cell(value));
    }
    let json = serde_json::to_string(value).expect("строка JSON");
    let escaped: String = json
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_string()
            } else {
                format!("&#{};", u32::from(c))
            }
        })
        .collect();
    format!("<code>{escaped}</code>")
}

pub fn render(lang: Lang, meta: &Meta, result: &BenchmarkResult) -> String {
    let l = labels(lang);
    let show = |value: f64, unit| format::quantity_in(value, unit, l.cores);
    let mut out = String::new();
    let _ = writeln!(out, "{}", marker(meta));
    let _ = writeln!(
        out,
        "### `{}` · {} `{}`\n",
        result.scenario.id, l.heading, result.profile.id
    );
    let ok = result
        .runs
        .iter()
        .filter(|r| r.status == RunStatus::Success)
        .count();
    let tool = |t: &benchmark_contract::result::Tool| format!("{} {}", t.name, t.version);
    let mut facts = vec![
        format!("{} UTC", &result.provenance.recorded_at[..10]),
        format!(
            "`{}` {}",
            result.scenario.module, result.provenance.code.module_version
        ),
        tool(&result.provenance.measurement),
    ];
    if let Some(runner) = &result.provenance.runner {
        facts.push(tool(runner));
    }
    facts.push(format!(
        "r{} · {} · {}",
        result.scenario.revision, result.profile.cache, result.profile.network
    ));
    facts.push((l.outcome)(ok, result.runs.len(), result.warmup_runs));
    if result.provenance.code.dirty != Some(false) {
        facts.push(l.dirty.to_owned());
    }
    let _ = writeln!(
        out,
        "{}{}{}{}\n",
        l.confirmed,
        l.colon,
        facts.join(" · "),
        l.stop
    );

    let _ = writeln!(out, "| {} | {} |\n|---|---|", l.environment, l.value);
    for (name, fact) in &result.environment {
        if let Fact::Known { value } = fact {
            let mark = if meta.environment.contains_key(name) {
                " ★"
            } else {
                ""
            };
            let _ = writeln!(out, "| `{name}`{mark} | {} |", markdown_table_cell(value));
        }
    }
    out.push('\n');

    // Параметры, влияющие на сопоставимость, — часть определения нагрузки
    // снимка: без них блок не говорит, что именно измерено.
    let mut sizes: Vec<(String, String)> = meta
        .parameters
        .iter()
        .map(|(name, value)| {
            (
                format!("{} `{name}`", l.parameter),
                markdown_parameter(value),
            )
        })
        .collect();
    let input_sizes: Vec<(String, String)> = result
        .inputs
        .iter()
        .flat_map(|input| {
            input
                .size
                .iter()
                .map(move |(name, q)| (format!("`{}` · `{name}`", input.id), show(q.value, q.unit)))
        })
        .collect();
    sizes.extend(input_sizes);
    if sizes.is_empty() {
        let _ = writeln!(out, "{}\n", l.no_workload);
    } else {
        let _ = writeln!(out, "| {} | {} |\n|---|---|", l.workload, l.value);
        for (name, value) in sizes {
            let _ = writeln!(out, "| {name} | {value} |");
        }
        out.push('\n');
    }

    let _ = writeln!(
        out,
        "| {} | {} | {} | {} | {} | n |\n|---|---|---|---|---|---|",
        l.metric, l.scope, l.mean, l.median, l.range
    );
    let mut missing = Vec::new();
    for metric in &result.metrics {
        match Aggregate::compute(&result.successful_values(&metric.key)) {
            Some(a) => {
                let spread = a
                    .stddev
                    .map(|s| format!(" ± {}", show(s, metric.unit)))
                    .unwrap_or_default();
                let partial = if metric.availability == Availability::Partial {
                    " · partial"
                } else {
                    ""
                };
                let _ = writeln!(
                    out,
                    "| `{}` | {} / {} | {}{spread}{partial} | {} | {} – {} | {} |",
                    metric.key,
                    metric.scope,
                    metric.source,
                    show(a.mean, metric.unit),
                    show(a.median, metric.unit),
                    show(a.min, metric.unit),
                    show(a.max, metric.unit),
                    a.count
                );
            }
            None => missing.push(format!(
                "`{}` — `{}`{}",
                metric.key,
                serde_json::to_value(metric.availability)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                metric
                    .reason
                    .as_deref()
                    .map(|r| format!(": {r}"))
                    .unwrap_or_default()
            )),
        }
    }
    out.push('\n');
    if !missing.is_empty() {
        let _ = writeln!(
            out,
            "{}{}{}{}\n",
            l.not_measured,
            l.colon,
            missing.join("; "),
            l.stop
        );
    }
    let _ = writeln!(out, "★ {}", l.reference);
    let _ = writeln!(out, "{END}");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_table_cell_escapes_backslashes_before_pipes() {
        assert_eq!(markdown_table_cell(r"a\|b"), r"a\\\|b");
    }

    #[test]
    fn arbitrary_parameter_text_stays_in_one_table_cell() {
        assert_eq!(markdown_parameter("4"), "`4`");
        assert_eq!(markdown_parameter("a|b"), r"`a\|b`");
        for value in [
            "a`b|c\\d\nnext",
            "a\\|b",
            "\r\n",
            "",
            " a ",
            "`*x*<&amp;>_[]",
        ] {
            let rendered = markdown_parameter(value);
            assert!(rendered.starts_with("<code>"), "{rendered}");
            assert!(!rendered.contains(['|', '`', '\n', '\r', '\\', '*', '_', '[', ']']));
            let encoded = rendered
                .strip_prefix("<code>")
                .unwrap()
                .strip_suffix("</code>")
                .unwrap();
            // Видимое содержимое — точная JSON-строка, включая escapes переводов строк.
            let mut decoded = String::new();
            let mut rest = encoded;
            while let Some(pos) = rest.find("&#") {
                decoded.push_str(&rest[..pos]);
                rest = &rest[pos + 2..];
                let end = rest.find(';').unwrap();
                decoded.push(char::from_u32(rest[..end].parse().unwrap()).unwrap());
                rest = &rest[end + 1..];
            }
            decoded.push_str(rest);
            assert_eq!(serde_json::from_str::<String>(&decoded).unwrap(), value);
        }
    }

    #[test]
    fn marker_never_contains_comment_terminator() {
        let meta = Meta {
            scenario: "a.b".into(),
            profile: "warm".into(),
            recorded_at: "2026-10-28T00:00:00Z".into(),
            environment: BTreeMap::from([("cpu_model".into(), "X -- Y --- Z".into())]),
            workload: None,
            parameters: BTreeMap::from([("writers".into(), "4 -- 8".into())]),
        };
        let text = marker(&meta);
        assert!(!text[4..text.len() - 3].contains("--"), "{text}");
        // Пустые параметры в маркер не пишутся: маркеры сценариев без них не меняются.
        let plain = Meta {
            parameters: BTreeMap::new(),
            ..meta.clone()
        };
        assert!(!marker(&plain).contains("parameters"));
        let parsed = blocks(&format!("{text}\nbody\n{END}\n")).unwrap();
        assert_eq!(parsed[0].meta, meta);
    }

    #[test]
    fn git_blob_sha_matches_git() {
        // `printf 'hello\n' | git hash-object --stdin`
        assert_eq!(
            git_blob_sha(b"hello\n"),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }
}
