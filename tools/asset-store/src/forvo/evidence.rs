use super::{Candidate, ForvoSession, RANKING, error, pauses_batch, provider_code, rank, word_url};
use crate::{
    AssetError, AssetRecord, AssetStore,
    hashing::sha256_hex,
    pronunciation::{self, Pronunciation},
};
use serde::{Deserialize, Serialize};

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
}
impl ForvoEvidence {
    pub fn validate(&self, request: &Pronunciation, sha: &str) -> Result<(), AssetError> {
        let invalid = || {
            error(
                "invalid_forvo_evidence",
                "Происхождение Forvo не связано с точной парой/скачанным SHA",
            )
        };
        if self.schema_version != 1
            || self.page_url != word_url(&request.surface)
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

/// Один порядок попыток для браузера и изолированных offline-сценариев.
pub(super) trait CandidateSource {
    async fn candidates(&mut self, request: &Pronunciation) -> Result<Vec<Candidate>, AssetError>;
    async fn download(
        &mut self,
        request: &Pronunciation,
        candidate: &Candidate,
    ) -> Result<Vec<u8>, AssetError>;
}
impl CandidateSource for ForvoSession {
    async fn candidates(&mut self, request: &Pronunciation) -> Result<Vec<Candidate>, AssetError> {
        ForvoSession::candidates(self, request).await
    }
    async fn download(
        &mut self,
        request: &Pronunciation,
        candidate: &Candidate,
    ) -> Result<Vec<u8>, AssetError> {
        ForvoSession::download(self, request, candidate).await
    }
}

/// Попытки задаёт рейтинг; отказ одного кандидата не означает отсутствие слова.
pub async fn acquire(
    session: &mut ForvoSession,
    request: &Pronunciation,
) -> Result<Acquisition, AssetError> {
    let first = acquire_from(session, request).await;
    match first {
        Err(e) if super::requires_interaction(&e) => {
            if session
                .recover_interactively(Some(&request.surface))
                .await?
            {
                acquire_from(session, request).await
            } else {
                Err(e)
            }
        }
        other => other,
    }
}
pub(super) async fn acquire_from(
    session: &mut impl CandidateSource,
    request: &Pronunciation,
) -> Result<Acquisition, AssetError> {
    use crate::browser_diagnostics::{BrowserItemContext, BrowserItemTimer};
    let timer = BrowserItemTimer::new(BrowserItemContext::without_generation(
        "forvo_web",
        &request.surface,
        1,
    ));
    let result = acquire_timed(session, request, &timer).await;
    match &result {
        Ok(_) => timer.finish_success(),
        Err(error) => timer.finish_failure(provider_code(error), pauses_batch(error), None),
    }
    result
}

async fn acquire_timed(
    session: &mut impl CandidateSource,
    request: &Pronunciation,
    timer: &crate::browser_diagnostics::BrowserItemTimer,
) -> Result<Acquisition, AssetError> {
    let mut candidates = None;
    for retry in 0..3 {
        let stage = timer.stage("candidates");
        let found = session.candidates(request).await;
        match &found {
            Ok(_) => stage.finish_success(),
            Err(error) => stage.finish_failure(provider_code(error), pauses_batch(error), None),
        }
        match found {
            Ok(items) => {
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
    let candidates = candidates.expect("bounded attempts");
    let mut attempts = Vec::new();
    for candidate in candidates.iter().filter(|c| c.download_available) {
        let stage = timer.stage("download_and_validate_mp3");
        let result = session
            .download(request, candidate)
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
        match result {
            Ok(bytes) => {
                attempts.push(Attempt {
                    record_id: candidate.record_id.clone(),
                    code: "downloaded".into(),
                });
                let evidence = ForvoEvidence {
                    schema_version: 1,
                    page_url: word_url(&request.surface),
                    downloaded_sha256: sha256_hex(&bytes),
                    selected_record_id: candidate.record_id.clone(),
                    ranking_policy: RANKING.into(),
                    candidates: candidates.clone(),
                    attempts,
                };
                evidence.validate(request, &sha256_hex(&bytes))?;
                return Ok(Acquisition { bytes, evidence });
            }
            Err(mut e) if pauses_batch(&e) => {
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
        .validate(request, &sha256_hex(&acquisition.bytes))?;
    pronunciation::import_forvo_bytes(
        store,
        request,
        &acquisition.bytes,
        &acquisition.evidence,
        replace_expected,
    )
}
