//! Адресный офлайн-обзор аудиокандидатов: `review-export` → каталог
//! `review-<review_id>/{snapshot.json,index.html,items.jsonl,decisions.tsv,<sha>.mp3}`
//! → `review-apply`.
//!
//! Страница позволяет отметить выбор прямо рядом с плеером и сохранить файл
//! решений того же формата (`decisions-<review_id>.tsv`); `review-apply
//! --decisions-file` проверяет его теми же правилами, что и `decisions.tsv`.
//! Ни страница, ни её скрипт не источник доверия: решение — только явная
//! первая колонка, сверенная с текущим состоянием пакета.
//!
//! Жизненный цикл снимка (адресация, точные копии, неизменяемые колонки,
//! повторный export без перезаписи решений) принадлежит общему
//! `review_snapshot`. HTML — только просмотр: он не источник доверия, ничего
//! не публикует и не запускает действий; решение — первая колонка
//! `decisions.tsv`, которую применяет только `review-apply` после сверки всех
//! строк с текущим состоянием пакета.

use std::fmt::Write as _;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::engine::{self, SelectionIndex};
use super::model::*;
use super::ops::{
    DecideAction, GroupFilter, candidate_bytes, group_view, recorded_at, validate_decision,
};
use crate::batch::html_escape;
use crate::batch_runtime::{RuntimeBatchState, SafeBatchRuntime};
use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;
use crate::model::{AssetIdentity, ValidatorIdentity};
use crate::pronunciation::Pronunciation;
use crate::review_snapshot::{ReviewCell, ReviewFormat, ReviewSnapshot};
use crate::store::AssetStore;

const SCHEMA: u32 = 2;
pub const MAX_PAGE_SIZE: usize = 500;

const FORMAT: ReviewFormat = ReviewFormat {
    schema: SCHEMA,
    comments: &[
        "# Обзор аудиокандидатов произношения, формат v2",
        "# Первая колонка: N = выбрать вариант N; heard:N = выбрать вариант N, я прослушал запись;",
        "#   rN = отклонить вариант N (исходник не удаляется); defer = отложить; - или пусто = без решения.",
        "# Остальные колонки неизменны; разделитель TAB. Изменение HTML ничего не применяет.",
    ],
};

fn validator() -> ValidatorIdentity {
    ValidatorIdentity::new("pronunciation-selection-policy", "1").expect("постоянная версия")
}

/// Вариант строки: точный кандидат и его копия в каталоге снимка.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewOption {
    pub number: u32,
    pub candidate_id: String,
    pub kind: CandidateKind,
    pub addressed_sha256: String,
    pub source_sha256: String,
    pub span: Option<CandidateSpan>,
    pub technical_usable: bool,
    pub class: SelectionClass,
    pub proof: PhoneticProof,
    /// Файл, который проигрывается: байты кандидата или исходник интервала.
    pub file: String,
    pub file_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AudioReviewRow {
    pub identity: AssetIdentity,
    pub surface: String,
    pub reading: String,
    pub class: SelectionClass,
    pub fingerprint: String,
    pub recommended: Option<String>,
    pub options: Vec<ReviewOption>,
    /// Сведения только для отображения (ASR, причины, происхождение).
    pub display: serde_json::Value,
}

impl crate::review_snapshot::ReviewRow for AudioReviewRow {
    fn identity(&self) -> &AssetIdentity {
        &self.identity
    }
    fn image(&self) -> Option<(&str, &str)> {
        None
    }
    fn files(&self) -> Vec<(&str, &str)> {
        let mut files: Vec<(&str, &str)> = self
            .options
            .iter()
            .map(|o| (o.file.as_str(), o.file_sha256.as_str()))
            .collect();
        files.sort();
        files.dedup();
        files
    }
    fn columns(&self) -> String {
        let options = self
            .options
            .iter()
            .map(|o| {
                let span = o
                    .span
                    .map(|s| format!("[{:.0}-{:.0}ms]", s.start_ms(), s.end_ms()))
                    .unwrap_or_default();
                format!(
                    "{}={}:{}@{}{span}",
                    o.number,
                    o.candidate_id,
                    o.kind.as_str(),
                    o.addressed_sha256
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "{}\t{}\t{}\t{}\t{}",
            self.identity.key,
            self.class.as_str(),
            self.fingerprint,
            self.recommended.as_deref().unwrap_or("-"),
            if options.is_empty() { "-" } else { &options }
        )
    }
    fn validate(&self) -> Result<(), AssetError> {
        let pronunciation = Pronunciation::new(&self.surface, &self.reading)?;
        if pronunciation.identity() != self.identity
            || !valid_sha(&self.fingerprint)
            || self.options.iter().enumerate().any(|(i, o)| {
                o.number as usize != i + 1
                    || !valid_sha(&o.addressed_sha256)
                    || !valid_sha(&o.file_sha256)
                    || o.file != format!("{}.mp3", o.file_sha256)
            })
        {
            return Err(invalid("строка обзора аудио противоречива"));
        }
        Ok(())
    }
}

type AudioSnapshot = ReviewSnapshot<AudioReviewRow>;

#[derive(Debug, Serialize)]
pub struct ExportReport {
    pub review_id: String,
    pub directory: String,
    pub decisions: String,
    pub index: String,
    /// Путь страницы для браузера Windows, если команда выполнена в WSL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_windows: Option<String>,
    pub rows: usize,
    pub matched: usize,
    pub page: usize,
    pub pages: usize,
    /// Короткая инструкция пользователю ручного режима.
    pub user_instructions: Vec<String>,
}

/// Путь `\\wsl.localhost\<distro>\…` для браузера Windows.
fn windows_path(path: &std::path::Path) -> Option<String> {
    let distro = std::env::var("WSL_DISTRO_NAME")
        .ok()
        .filter(|d| !d.is_empty())?;
    let text = path.to_str()?;
    text.starts_with('/')
        .then(|| format!("\\\\wsl.localhost\\{distro}{}", text.replace('/', "\\")))
}

fn rows_for(
    batch: &SelectionBatch,
    filter: GroupFilter,
    page: usize,
    page_size: usize,
) -> Result<(Vec<AudioReviewRow>, usize), AssetError> {
    let index = SelectionIndex::new(batch);
    let recommendations = engine::recommend_all_with_index(&index, batch);
    let selected: Vec<&GroupRecommendation> = recommendations
        .values()
        .filter(|r| {
            filter.accepts(
                &index,
                batch,
                r,
                batch.current_decision(&r.surface, &r.reading),
            )
        })
        .collect();
    let matched = selected.len();
    let mut rows = Vec::new();
    for recommendation in selected
        .into_iter()
        .skip(page.saturating_mul(page_size))
        .take(page_size)
    {
        let mut options = Vec::new();
        for assessment in &recommendation.candidates {
            let (link, candidate) = index
                .find_candidate(&assessment.candidate_id)
                .expect("оценка ссылается на кандидата пакета");
            let file_sha = candidate
                .audio
                .as_ref()
                .map_or(link.source_sha256.clone(), |a| a.sha256.clone());
            options.push(ReviewOption {
                number: options.len() as u32 + 1,
                candidate_id: candidate.candidate_id.clone(),
                kind: candidate.kind,
                addressed_sha256: candidate.addressed_sha(&link.source_sha256).into(),
                source_sha256: link.source_sha256.clone(),
                span: candidate.span,
                technical_usable: candidate.technical.usable,
                class: assessment.class,
                proof: assessment.proof,
                file: format!("{file_sha}.mp3"),
                file_sha256: file_sha,
            });
        }
        let view = group_view(&index, batch, recommendation);
        let display = serde_json::to_value(&view).map_err(|e| invalid(e.to_string()))?;
        rows.push(AudioReviewRow {
            identity: Pronunciation::new(&recommendation.surface, &recommendation.reading)?
                .identity(),
            surface: recommendation.surface.clone(),
            reading: recommendation.reading.clone(),
            class: recommendation.class,
            fingerprint: recommendation.fingerprint.clone(),
            recommended: recommendation.recommended.clone(),
            options,
            display,
        });
    }
    Ok((rows, matched))
}

/// Создаёт (или повторно открывает) снимок выбранной страницы целей.
pub fn export(
    store: &AssetStore,
    batch_id: &str,
    filter: GroupFilter,
    page: usize,
    page_size: usize,
) -> Result<ExportReport, AssetError> {
    if page_size == 0 || page_size > MAX_PAGE_SIZE {
        return Err(invalid(format!("размер страницы: 1–{MAX_PAGE_SIZE}")));
    }
    let mut runtime = SafeBatchRuntime::open(store.root(), batch_id)?;
    let batch: SelectionBatch = runtime
        .load()?
        .ok_or_else(|| invalid("пакет выбора не найден"))?;
    let (rows, matched) = rows_for(&batch, filter, page, page_size)?;
    if rows.is_empty() {
        return Err(invalid("нет целей, подходящих под фильтр и страницу"));
    }
    let snapshot = AudioSnapshot {
        schema_version: SCHEMA,
        batch_id: batch.batch_id.clone(),
        validator: validator(),
        batch_revision: Some(batch.revision),
        rows,
    };
    let (bundle, directory) = crate::review_snapshot::export(
        |name| runtime.artifact_directory(name),
        &snapshot,
        &FORMAT,
        |row, filename| {
            let option = row
                .options
                .iter()
                .find(|o| o.file == filename)
                .ok_or_else(|| invalid("файл обзора не принадлежит строке"))?;
            let (link, candidate) = batch
                .find_candidate(&option.candidate_id)
                .ok_or_else(|| invalid("кандидат обзора отсутствует"))?;
            candidate_bytes(&runtime, &batch, link, candidate)
        },
    )?;
    let mut items = String::new();
    for row in &snapshot.rows {
        items.push_str(&serde_json::to_string(row).map_err(|e| invalid(e.to_string()))?);
        items.push('\n');
    }
    directory.write("items.jsonl", items.as_bytes())?;
    let template = crate::review_snapshot::decisions_template(&snapshot, &FORMAT, &bundle.id);
    directory.write(
        "index.html",
        render(&snapshot, &bundle.id, page, matched, page_size, &template).as_bytes(),
    )?;
    let index = bundle.directory.join("index.html");
    let index_windows = windows_path(&index);
    let user_instructions = vec![
        format!(
            "Откройте в браузере: {}",
            index_windows
                .clone()
                .unwrap_or_else(|| index.display().to_string())
        ),
        "Прослушайте варианты каждой цели и отметьте «выбрать», «отклонить» или «отложить»; «я прослушал» — только если действительно слушали.".into(),
        format!(
            "Нажмите «Завершить и сохранить решения»: браузер сохранит decisions-{}.tsv. Сообщите агенту путь к этому файлу — агент применит только отмеченное.",
            &bundle.id
        ),
    ];
    Ok(ExportReport {
        review_id: bundle.id,
        decisions: bundle.decisions.display().to_string(),
        index: index.display().to_string(),
        index_windows,
        directory: bundle.directory.display().to_string(),
        rows: snapshot.rows.len(),
        matched,
        page,
        pages: matched.div_ceil(page_size),
        user_instructions,
    })
}

#[derive(Debug, Clone, PartialEq)]
enum Cell {
    Select { number: u32, heard: bool },
    Reject(u32),
    Defer,
}

fn parse_cell(row: &AudioReviewRow, cell: ReviewCell) -> Result<Option<Cell>, AssetError> {
    let action = match cell {
        ReviewCell::Unspecified | ReviewCell::Skip => return Ok(None),
        ReviewCell::Action(action) => action,
    };
    let number = |text: &str| -> Result<u32, AssetError> {
        text.parse::<u32>()
            .ok()
            .filter(|n| text.bytes().all(|b| b.is_ascii_digit()) && *n >= 1)
            .filter(|n| (*n as usize) <= row.options.len())
            .ok_or_else(|| {
                AssetError::new(
                    ErrorCode::IdentityConflict,
                    format!(
                        "{}: номер варианта должен быть из колонки вариантов этой строки",
                        row.identity.key
                    ),
                )
            })
    };
    Ok(Some(match action.as_str() {
        "defer" => Cell::Defer,
        other => {
            if let Some(rest) = other.strip_prefix("heard:") {
                Cell::Select {
                    number: number(rest)?,
                    heard: true,
                }
            } else if let Some(rest) = other.strip_prefix('r') {
                Cell::Reject(number(rest)?)
            } else {
                Cell::Select {
                    number: number(other)?,
                    heard: false,
                }
            }
        }
    }))
}

#[derive(Debug, Default, Serialize)]
pub struct ApplyReport {
    pub applied: usize,
    pub selected: usize,
    pub rejected: usize,
    pub deferred: usize,
    pub skipped: usize,
    pub already_applied: usize,
    pub changed: bool,
    pub revision: u64,
}

/// Проверяет все строки до единственной записи состояния; любой отказ
/// оставляет пакет без изменений.
pub fn apply(
    store: &AssetStore,
    batch_id: &str,
    review_id: &str,
) -> Result<ApplyReport, AssetError> {
    apply_with(store, batch_id, review_id, None)
}

/// Как [`apply`], но решения берутся из отдельного файла, сохранённого
/// страницей обзора (тот же формат и те же проверки снимка).
pub fn apply_with(
    store: &AssetStore,
    batch_id: &str,
    review_id: &str,
    decisions: Option<&str>,
) -> Result<ApplyReport, AssetError> {
    let mut runtime = SafeBatchRuntime::open(store.root(), batch_id)?;
    let mut batch: SelectionBatch = runtime
        .load()?
        .ok_or_else(|| invalid("пакет выбора не найден"))?;
    let (snapshot, cells) = crate::review_snapshot::read_with_decisions::<AudioReviewRow>(
        |name| runtime.artifact_directory(name),
        runtime.batch_id(),
        review_id,
        &FORMAT,
        decisions,
    )?;
    if snapshot.validator != validator() {
        return Err(invalid("политика обзора устарела"));
    }
    let snapshot_revision = snapshot
        .batch_revision
        .ok_or_else(|| invalid("обзор устарел: ревизия пакета отсутствует"))?;
    let subsequent: Vec<_> = batch
        .decisions
        .iter()
        .filter(|d| d.state_revision > snapshot_revision)
        .collect();
    let replay = snapshot_revision.checked_add(1) == Some(batch.revision)
        && !subsequent.is_empty()
        && subsequent.iter().all(|d| {
            matches!(&d.authority,
            DecisionAuthority::UserDirectReview { review_id: id, .. } if id == review_id)
        });
    if batch.revision != snapshot_revision && !replay {
        return Err(invalid("обзор устарел: ревизия пакета изменилась"));
    }
    // При replay восстанавливаем состояние до единственного атомарного apply.
    // Любая иная ревизия запрещена; новые действия в старом TSV запрещены ниже.
    let base_on_replay = replay.then(|| {
        let mut base = batch.clone();
        base.decisions
            .retain(|d| d.state_revision <= snapshot_revision);
        base.revision = snapshot_revision;
        base
    });
    let base = base_on_replay.as_ref().unwrap_or(&batch);
    let index = SelectionIndex::new(base);
    let mut report = ApplyReport::default();
    let mut decisions = Vec::new();
    for (row, cell) in snapshot.rows.iter().zip(cells) {
        let recommendation = engine::recommend_with(&index, base, &row.surface, &row.reading);
        if recommendation.fingerprint != row.fingerprint {
            return Err(invalid(format!(
                "{}: обзор устарел: кандидаты, свидетельства или экспертизы цели изменились",
                row.identity.key
            )));
        }
        // Целостность исходных blob относится ко всему снимку, даже если в
        // строке пока нет решения. Копии проверяет общий review_snapshot.
        for option in &row.options {
            let (link, candidate) = index
                .find_candidate(&option.candidate_id)
                .ok_or_else(|| invalid("кандидат обзора отсутствует"))?;
            if sha256_hex(&candidate_bytes(&runtime, &batch, link, candidate)?)
                != option.file_sha256
            {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    "байты кандидата изменились относительно снимка",
                ));
            }
        }
        let Some(cell) = parse_cell(row, cell)? else {
            report.skipped += 1;
            continue;
        };
        let option = |number: u32| &row.options[number as usize - 1];
        let (action, heard) = match &cell {
            Cell::Select { number, heard } => (
                DecideAction::Select {
                    candidate_id: option(*number).candidate_id.clone(),
                    expected_sha256: option(*number).addressed_sha256.clone(),
                },
                *heard,
            ),
            Cell::Reject(number) => (
                DecideAction::Reject {
                    candidate_id: option(*number).candidate_id.clone(),
                    expected_sha256: option(*number).addressed_sha256.clone(),
                },
                false,
            ),
            Cell::Defer => (DecideAction::Defer, false),
        };
        let authority = DecisionAuthority::UserDirectReview {
            review_id: review_id.into(),
            user_listened: heard,
        };
        let decision_action = validate_decision(base, &recommendation, &action, &authority)?;
        if let Some(current) = batch.current_decision(&row.surface, &row.reading)
            && current.action == decision_action
            && current.authority == authority
        {
            report.already_applied += 1;
            continue;
        }
        if replay {
            return Err(invalid(
                "повторное применение не совпадает с прежним решением",
            ));
        }
        match &cell {
            Cell::Select { .. } => report.selected += 1,
            Cell::Reject(_) => report.rejected += 1,
            Cell::Defer => report.deferred += 1,
        }
        report.applied += 1;
        decisions.push(SelectionDecision {
            decision_id: String::new(),
            surface: row.surface.clone(),
            reading: row.reading.clone(),
            action: decision_action,
            authority,
            reason: format!("review:{review_id}"),
            group_fingerprint: row.fingerprint.clone(),
            state_revision: 0,
            recorded_at_unix_ms: None,
        });
    }
    if replay && report.already_applied != subsequent.len() {
        return Err(invalid("повторное применение пропускает прежнее решение"));
    }
    if !decisions.is_empty() {
        let revision = batch.revision + 1;
        let timestamp = recorded_at(std::time::SystemTime::now())?;
        for mut decision in decisions {
            decision.decision_id = batch.next_decision_id();
            decision.state_revision = revision;
            decision.recorded_at_unix_ms = Some(timestamp);
            batch.decisions.push(decision);
        }
        batch.revision = revision;
        batch.validate()?;
        runtime.save(&batch)?;
        report.changed = true;
    }
    report.revision = batch.revision;
    Ok(report)
}

/// Скрипт повторов, замедления и сборки файла решений; CSP разрешает только
/// его точный SHA-256. Скрипт заполняет первую колонку встроенного шаблона
/// `decisions.tsv` и отдаёт файл браузеру; сам он ничего не применяет.
const PLAYER_SCRIPT: &str = "document.addEventListener('click',function(e){var b=e.target.closest('button[data-for]');if(!b)return;var a=document.getElementById(b.getAttribute('data-for'));if(!a)return;var r=b.getAttribute('data-rate');if(r){a.playbackRate=parseFloat(r);return;}if(b.hasAttribute('data-loop')){a.loop=!a.loop;b.setAttribute('aria-pressed',a.loop?'true':'false');return;}a.currentTime=parseFloat(a.getAttribute('data-start')||'0');a.play();});(function(){var t=document.getElementById('decisions-template'),s=document.getElementById('save-decisions');if(!t||!s)return;var tpl=JSON.parse(t.textContent);function build(){var lines=tpl.split('\n'),row=-1,out=[];for(var i=0;i<lines.length;i++){var l=lines[i];if(l.trim()===''||l.charAt(0)==='#'){out.push(l);continue;}if(row<0){row=0;out.push(l);continue;}var c=document.querySelector('input[name=d-'+row+']:checked'),v=c?c.value:'',h=document.getElementById('heard-'+row);if(/^[0-9]+$/.test(v)&&h&&h.checked)v='heard:'+v;var k=l.indexOf('\t');out.push(v+l.slice(k));row++;}return out.join('\n');}s.addEventListener('click',function(){var text=build(),o=document.getElementById('decisions-output');o.value=text;o.hidden=false;var a=document.createElement('a');a.href=URL.createObjectURL(new Blob([text],{type:'text/tab-separated-values'}));a.download=s.getAttribute('data-file');document.body.appendChild(a);a.click();a.remove();});})();";

fn script_hash() -> String {
    STANDARD.encode(Sha256::digest(PLAYER_SCRIPT.as_bytes()))
}

fn json_text(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

fn render(
    snapshot: &AudioSnapshot,
    review_id: &str,
    page: usize,
    matched: usize,
    page_size: usize,
    template: &str,
) -> String {
    let mut html = format!(
        "<!doctype html><html lang=\"ru\"><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; media-src 'self' file:; style-src 'unsafe-inline'; script-src 'sha256-{}'; base-uri 'none'; form-action 'none'\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Обзор аудиокандидатов</title><style>body{{font-family:system-ui,sans-serif;max-width:1100px;margin:auto;padding:1rem;color:#202124;background:#fff}}article{{border:1px solid #9aa0a6;border-radius:.5rem;padding:1rem;margin:1rem 0}}.opt{{border-top:1px solid #dadce0;padding:.75rem 0}}pre{{white-space:pre-wrap;overflow-wrap:anywhere;background:#f5f6f7;padding:.5rem;border-radius:.35rem;font-size:.85rem}}code{{overflow-wrap:anywhere}}.rec{{font-weight:700}}button{{margin-right:.25rem}}details{{margin:.5rem 0}}@media (prefers-color-scheme: dark){{body{{background:#1f1f1f;color:#e8eaed}}pre{{background:#2b2b2b}}}}</style></head><body><h1>Обзор аудиокандидатов</h1><p>Снимок <code>{}</code>; пакет <code>{}</code>; страница {} из {} ({} целей под фильтром).</p><p>Прослушайте варианты и отметьте решение у каждой нужной цели. Кнопка внизу сохранит файл решений <code>decisions-{}.tsv</code>; его применяет только <code>selection review-apply --decisions-file</code> после сверки со снимком. Пустая отметка — без решения. Можно и вручную заполнить первую колонку <code>decisions.tsv</code> рядом с этим файлом. Тон и омонимы не проверяются.</p>",
        script_hash(),
        html_escape(review_id),
        html_escape(&snapshot.batch_id),
        page.saturating_add(1),
        matched.div_ceil(page_size).max(1),
        matched,
        html_escape(review_id),
    );
    for (row_index, row) in snapshot.rows.iter().enumerate() {
        let display = &row.display;
        let recognition = serde_json::json!({"links": display["links"], "sources": display["sources"],
            "audio_reviews": display["audio_reviews"],
            "примечание": "null означает: нет данных; голоса Forvo не подтверждают фонемы"});
        write!(
            html,
            "<article id=\"row-{row_index}\"><h2>{} · {}</h2><p>Класс: <code>{}</code>; рекомендован: <code>{}</code></p><p>Причины: <code>{}</code></p><details><summary>Распознавание и происхождение</summary><pre>{}</pre></details>",
            html_escape(&row.surface),
            html_escape(&row.reading),
            row.class.as_str(),
            html_escape(row.recommended.as_deref().unwrap_or("нет")),
            html_escape(&display["recommendation"]["reasons"].to_string()),
            html_escape(&json_text(&recognition)),
        )
        .expect("запись в String");
        for option in &row.options {
            let player = format!("p-{row_index}-{}", option.number);
            let (src, start) = match option
                .span
                .filter(|_| option.kind == CandidateKind::OccurrenceInterval)
            {
                Some(span) => (
                    format!(
                        "{}#t={:.3},{:.3}",
                        option.file,
                        span.start_ms() / 1000.0,
                        span.end_ms() / 1000.0
                    ),
                    span.start_ms() / 1000.0,
                ),
                None => (option.file.clone(), 0.0),
            };
            let assessment = display["candidates"]
                .as_array()
                .and_then(|c| {
                    c.iter()
                        .find(|v| v["assessment"]["candidate_id"] == option.candidate_id.as_str())
                })
                .cloned()
                .unwrap_or_default();
            write!(
                html,
                "<div class=\"opt\"><p class=\"{}\">Вариант {} · <code>{}</code> · {} · класс <code>{}</code> · фонетика <code>{}</code> · техника {}</p><p>SHA: <code>{}</code>; исходник: <code>{}</code>{}</p><audio id=\"{player}\" controls preload=\"none\" data-start=\"{start:.3}\" src=\"{}\"></audio><div><button type=\"button\" data-for=\"{player}\">⟲ повтор</button><button type=\"button\" data-for=\"{player}\" data-rate=\"0.5\">0.5×</button><button type=\"button\" data-for=\"{player}\" data-rate=\"0.75\">0.75×</button><button type=\"button\" data-for=\"{player}\" data-rate=\"1\">1×</button><button type=\"button\" data-for=\"{player}\" data-loop aria-pressed=\"false\">цикл</button></div><details><summary>Свидетельства</summary><pre>{}</pre></details></div>",
                if row.recommended.as_deref() == Some(option.candidate_id.as_str()) {
                    "rec"
                } else {
                    ""
                },
                option.number,
                html_escape(&option.candidate_id),
                option.kind.as_str(),
                option.class.as_str(),
                html_escape(&serde_json::to_value(option.proof).map(|v| v.to_string()).unwrap_or_default()),
                if option.technical_usable { "пройдена" } else { "не пройдена" },
                html_escape(&option.addressed_sha256),
                html_escape(&option.source_sha256),
                option
                    .span
                    .map(|s| format!("; интервал {:.0}–{:.0} мс исходника", s.start_ms(), s.end_ms()))
                    .unwrap_or_default(),
                html_escape(&src),
                html_escape(&json_text(&assessment)),
            )
            .expect("запись в String");
        }
        write!(
            html,
            "<fieldset><legend>Решение</legend><label><input type=\"radio\" name=\"d-{row_index}\" value=\"\" checked> без решения</label>",
        )
        .expect("запись в String");
        for option in &row.options {
            write!(
                html,
                " <label><input type=\"radio\" name=\"d-{row_index}\" value=\"{n}\"> выбрать {n}</label> <label><input type=\"radio\" name=\"d-{row_index}\" value=\"r{n}\"> отклонить {n}</label>",
                n = option.number
            )
            .expect("запись в String");
        }
        write!(
            html,
            " <label><input type=\"radio\" name=\"d-{row_index}\" value=\"defer\"> отложить</label><br><label><input type=\"checkbox\" id=\"heard-{row_index}\"> я прослушал выбранный вариант</label></fieldset></article>",
        )
        .expect("запись в String");
    }
    let template_json = serde_json::to_string(template)
        .expect("строка сериализуема")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026");
    write!(
        html,
        "<p><button type=\"button\" id=\"save-decisions\" data-file=\"decisions-{id}.tsv\">Завершить и сохранить решения</button></p><textarea id=\"decisions-output\" rows=\"8\" cols=\"100\" readonly hidden></textarea><script type=\"application/json\" id=\"decisions-template\">{template_json}</script><script>{PLAYER_SCRIPT}</script></body></html>",
        id = html_escape(review_id),
    )
    .expect("запись в String");
    html
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csp_hash_matches_the_only_script() {
        let snapshot = AudioSnapshot {
            schema_version: SCHEMA,
            batch_id: "b".into(),
            validator: validator(),
            batch_revision: Some(0),
            rows: Vec::new(),
        };
        let html = render(&snapshot, &"0".repeat(64), 0, 0, 10, "# </script><x>\n");
        assert_eq!(html.matches("<script>").count(), 1);
        assert!(!html.contains("</script><x>"));
        assert!(html.contains(&format!("'sha256-{}'", script_hash())));
        assert!(!html.contains("http://") && !html.contains("https://"));
        assert!(!html.contains("autoplay"));
    }
}
