use super::{Candidate, ForvoSession, RANKING, error, pauses_batch, provider_code, rank, word_url};
use crate::{
    AssetError, AssetRecord, AssetStore,
    hashing::sha256_hex,
    pronunciation::{self, Pronunciation},
};
use serde::{Deserialize, Serialize};

/// Маркер исторической схемы 2; используется только для чтения свидетельств.
pub const COLLECT_ALL_MODE: &str = "collect_all_exact_word_ja_v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attempt {
    pub record_id: String,
    pub code: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForvoEvidence {
    pub schema_version: u32,
    pub page_url: String,
    pub downloaded_sha256: String,
    pub selected_record_id: String,
    pub ranking_policy: String,
    pub candidates: Vec<Candidate>,
    pub attempts: Vec<Attempt>,
    /// Только у схемы 2: режим получения. Схема 1 — выбор одного
    /// диктора по рейтингу с перечнем попыток до успешной.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acquisition_mode: Option<String>,
}
impl ForvoEvidence {
    /// Свидетельство связано только с написанием страницы и точным SHA:
    /// чтение в получении Forvo не участвует.
    pub fn validate(&self, surface: &str, sha: &str) -> Result<(), AssetError> {
        let invalid = || {
            error(
                "invalid_forvo_evidence",
                "Происхождение Forvo не связано с точным написанием/скачанным SHA",
            )
        };
        let collected = match (self.schema_version, self.acquisition_mode.as_deref()) {
            (1, None) => false,
            (2, Some(COLLECT_ALL_MODE)) => true,
            _ => return Err(invalid()),
        };
        if self.page_url != word_url(surface)
            || self.downloaded_sha256 != sha
            || sha.len() != 64
            || !sha
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.ranking_policy != RANKING
            || self.candidates.is_empty()
            || self.candidates.len() > 128
        {
            return Err(invalid());
        }
        let mut candidates = self.candidates.clone();
        rank(&mut candidates);
        if candidates != self.candidates {
            return Err(invalid());
        }
        let mut ids = std::collections::BTreeSet::new();
        for c in &candidates {
            c.validate()?;
            if !ids.insert(&c.record_id) {
                return Err(invalid());
            }
        }
        let available = candidates
            .iter()
            .filter(|c| c.download_available)
            .collect::<Vec<_>>();
        let Some(selected) = available
            .iter()
            .position(|c| c.record_id == self.selected_record_id)
        else {
            return Err(invalid());
        };
        if collected {
            // Полный сбор: свидетельство описывает ровно одну скачанную запись;
            // неудачи других записей хранит отчёт сбора, а не чужой SHA.
            if self.attempts
                != [Attempt {
                    record_id: self.selected_record_id.clone(),
                    code: "downloaded".into(),
                }]
            {
                return Err(invalid());
            }
            return Ok(());
        }
        if self.attempts.len() != selected + 1 {
            return Err(invalid());
        }
        for (index, (attempt, c)) in self.attempts.iter().zip(available).enumerate() {
            if attempt.record_id != c.record_id
                || attempt.code.is_empty()
                || attempt.code.len() > 80
                || !attempt
                    .code
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'_' || b.is_ascii_digit())
                || (attempt.code == "downloaded") != (index == selected)
            {
                return Err(invalid());
            }
        }
        Ok(())
    }
}

pub struct Acquisition {
    pub bytes: Vec<u8>,
    pub evidence: ForvoEvidence,
}

/// Один порядок попыток для браузера и изолированных автономных сценариев.
///
/// Адаптер получает только написание: точная страница слова, японский блок,
/// ID записи и кнопка Download проверяются по нему. Чтение сюда не передаётся.
pub(crate) trait CandidateSource {
    async fn candidates(&mut self, surface: &str) -> Result<Vec<Candidate>, AssetError>;
    async fn download(
        &mut self,
        surface: &str,
        candidate: &Candidate,
    ) -> Result<Vec<u8>, AssetError>;
}
impl CandidateSource for ForvoSession {
    async fn candidates(&mut self, surface: &str) -> Result<Vec<Candidate>, AssetError> {
        ForvoSession::candidates(self, surface).await
    }
    async fn download(
        &mut self,
        surface: &str,
        candidate: &Candidate,
    ) -> Result<Vec<u8>, AssetError> {
        ForvoSession::download(self, surface, candidate).await
    }
}

/// Попытки задаёт рейтинг; отказ одного кандидата не означает отсутствие слова.
pub async fn acquire(session: &mut ForvoSession, surface: &str) -> Result<Acquisition, AssetError> {
    let first = acquire_from(session, surface).await;
    match first {
        Err(e) if super::requires_interaction(&e) => {
            if session.recover_interactively(Some(surface)).await? {
                acquire_from(session, surface).await
            } else {
                Err(e)
            }
        }
        other => other,
    }
}
pub(super) async fn acquire_from(
    session: &mut impl CandidateSource,
    surface: &str,
) -> Result<Acquisition, AssetError> {
    use crate::browser_diagnostics::{BrowserItemContext, BrowserItemTimer};
    let timer = BrowserItemTimer::new(BrowserItemContext::without_generation(
        "forvo_web",
        surface,
        1,
    ));
    let surface = crate::pronunciation::selection::normalize_forvo_surface(surface)?;
    let result = acquire_timed(session, &surface, &timer).await;
    match &result {
        Ok(_) => timer.finish_success(),
        Err(error) => timer.finish_failure(provider_code(error), pauses_batch(error), None),
    }
    result
}

/// Кандидаты точной страницы с ограниченным повтором временных отказов сайта.
async fn ranked_candidates(
    session: &mut impl CandidateSource,
    surface: &str,
    timer: &crate::browser_diagnostics::BrowserItemTimer,
) -> Result<Vec<Candidate>, AssetError> {
    let mut candidates = None;
    for retry in 0..3 {
        let stage = timer.stage("candidates");
        let found = session.candidates(surface).await;
        match &found {
            Ok(_) => stage.finish_success(),
            Err(error) => stage.finish_failure(provider_code(error), pauses_batch(error), None),
        }
        match found {
            Ok(mut items) => {
                rank(&mut items);
                candidates = Some(items);
                break;
            }
            Err(e)
                if matches!(provider_code(&e), "rate_limited" | "site_unavailable")
                    && retry < 2 =>
            {
                tokio::time::sleep(std::time::Duration::from_secs(1 << retry)).await;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(candidates.expect("ограниченное число попыток"))
}

/// Загрузка одной записи и полная техническая проверка её байтов MP3.
async fn download_checked(
    session: &mut impl CandidateSource,
    surface: &str,
    candidate: &Candidate,
    timer: &crate::browser_diagnostics::BrowserItemTimer,
) -> Result<Vec<u8>, AssetError> {
    let stage = timer.stage("download_and_validate_mp3");
    let result = session
        .download(surface, candidate)
        .await
        .and_then(|bytes| {
            pronunciation::probe_mp3(&bytes).map_err(|e| {
                error(
                    if e.code == crate::ErrorCode::ValidatorFailure
                        && e.details["blocker"].is_null()
                    {
                        "invalid_mp3"
                    } else {
                        "technical_validation_failed"
                    },
                    "Скачанный файл не прошёл полную техническую проверку MP3",
                )
            })?;
            Ok(bytes)
        });
    match &result {
        Ok(_) => stage.finish_success(),
        Err(error) => stage.finish_failure(provider_code(error), pauses_batch(error), None),
    }
    result
}

async fn acquire_timed(
    session: &mut impl CandidateSource,
    surface: &str,
    timer: &crate::browser_diagnostics::BrowserItemTimer,
) -> Result<Acquisition, AssetError> {
    let candidates = ranked_candidates(session, surface, timer).await?;
    let mut attempts = Vec::new();
    for candidate in candidates.iter().filter(|c| c.download_available) {
        let result = download_checked(session, surface, candidate, timer).await;
        match result {
            Ok(bytes) => {
                attempts.push(Attempt {
                    record_id: candidate.record_id.clone(),
                    code: "downloaded".into(),
                });
                let evidence = ForvoEvidence {
                    schema_version: 1,
                    page_url: word_url(surface),
                    downloaded_sha256: sha256_hex(&bytes),
                    selected_record_id: candidate.record_id.clone(),
                    ranking_policy: RANKING.into(),
                    candidates: candidates.clone(),
                    attempts,
                    acquisition_mode: None,
                };
                evidence.validate(surface, &sha256_hex(&bytes))?;
                return Ok(Acquisition { bytes, evidence });
            }
            Err(mut e) if !super::rejects_candidate(&e) => {
                attempts.push(Attempt {
                    record_id: candidate.record_id.clone(),
                    code: provider_code(&e).into(),
                });
                e.details["candidates"] = serde_json::json!(candidates);
                e.details["attempts"] = serde_json::json!(attempts);
                return Err(e);
            }
            Err(e) => {
                tracing::debug!(event = "forvo_candidate_failed", record_id = %candidate.record_id, code = provider_code(&e), "Попытка получения записи Forvo отклонена");
                attempts.push(Attempt {
                    record_id: candidate.record_id.clone(),
                    code: provider_code(&e).into(),
                });
            }
        }
    }
    Err(AssetError::with_details(
        crate::ErrorCode::InvalidTransition,
        "У подходящих дикторов не удалось получить проверенный MP3",
        serde_json::json!({"provider":"forvo_web","code":"candidates_exhausted","candidates":candidates,"attempts":attempts,"next_action":"Проверьте записи вручную или используйте локальный import"}),
    ))
}

pub fn import_acquisition(
    store: &AssetStore,
    request: &Pronunciation,
    acquisition: &Acquisition,
    replace_expected: Option<String>,
) -> Result<AssetRecord, AssetError> {
    acquisition
        .evidence
        .validate(&request.surface, &sha256_hex(&acquisition.bytes))?;
    pronunciation::import_forvo_bytes(
        store,
        request,
        &acquisition.bytes,
        &acquisition.evidence,
        replace_expected,
    )
}
