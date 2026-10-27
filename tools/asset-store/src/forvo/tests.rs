use super::*;
use crate::pronunciation::Pronunciation;
use chromiumoxide::cdp::browser_protocol::browser::{
    DownloadProgressState, EventDownloadProgress, EventDownloadWillBegin,
};

struct FakeSource {
    candidates: Vec<Candidate>,
    responses: std::collections::VecDeque<Result<Vec<u8>, AssetError>>,
    calls: Vec<String>,
    candidate_calls: usize,
    candidate_error: Option<&'static str>,
}
impl evidence::CandidateSource for FakeSource {
    async fn candidates(&mut self, _: &Pronunciation) -> Result<Vec<Candidate>, AssetError> {
        self.candidate_calls += 1;
        match self.candidate_error {
            Some(code) => Err(error(code, "Повторите позже")),
            None => Ok(self.candidates.clone()),
        }
    }
    async fn download(
        &mut self,
        _: &Pronunciation,
        candidate: &Candidate,
    ) -> Result<Vec<u8>, AssetError> {
        self.calls.push(candidate.record_id.clone());
        self.responses.pop_front().expect("запланированный исход")
    }
}
fn source(responses: Vec<Result<Vec<u8>, AssetError>>) -> FakeSource {
    FakeSource {
        candidates: (1..=3)
            .map(|i| candidate(&i.to_string(), Some(4 - i), Some(Gender::Male), None))
            .collect(),
        responses: responses.into(),
        calls: Vec::new(),
        candidate_calls: 0,
        candidate_error: None,
    }
}
fn synthetic_mp3() -> Vec<u8> {
    let workspace = crate::temp_workspace::TempWorkspace::create("forvo-synthetic-mp3").unwrap();
    let path = workspace.path().join("fixture.mp3");
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-nostdin",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=0.6:sample_rate=44100",
            "-c:a",
            "libmp3lame",
            "-q:a",
            "4",
        ])
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
    let bytes = std::fs::read(path).unwrap();
    workspace.close().unwrap();
    bytes
}
#[tokio::test]
async fn fallback_rejects_aborted_download_and_html_before_accepting_original_mp3() {
    let bytes = synthetic_mp3();
    let mut source = source(vec![
        Err(error("download_aborted", "Загрузка отменена")),
        Ok(b"<html>login form</html>".to_vec()),
        Ok(bytes.clone()),
    ]);
    let request = Pronunciation::new("こいつ", "こいつ").unwrap();
    let result = evidence::acquire_from(&mut source, &request).await.unwrap();
    assert_eq!(source.calls, ["1", "2", "3"]);
    assert_eq!(result.bytes, bytes);
    assert_eq!(
        result
            .evidence
            .attempts
            .iter()
            .map(|a| a.code.as_str())
            .collect::<Vec<_>>(),
        ["download_aborted", "invalid_mp3", "downloaded"]
    );
    result
        .evidence
        .validate(&request, &crate::hashing::sha256_hex(&bytes))
        .unwrap();
}
#[tokio::test]
async fn blocked_session_stops_candidates_and_retains_attempt_report() {
    let mut source = source(vec![Err(error("session_expired", "Повторите login"))]);
    let error = evidence::acquire_from(&mut source, &Pronunciation::new("元気", "げんき").unwrap())
        .await
        .err()
        .expect("блокер");
    assert_eq!(provider_code(&error), "session_expired");
    assert_eq!(source.calls, ["1"]);
    let failure: batch::Failure = error.into();
    assert_eq!(failure.report.unwrap().attempts[0].code, "session_expired");
}
#[tokio::test]
async fn throttling_has_bounded_retries_and_never_becomes_not_found() {
    let mut source = source(Vec::new());
    source.candidate_error = Some("rate_limited");
    let error = evidence::acquire_from(&mut source, &Pronunciation::new("元気", "げんき").unwrap())
        .await
        .err()
        .expect("блокер");
    assert_eq!(source.candidate_calls, 3);
    assert!(source.calls.is_empty());
    assert_eq!(provider_code(&error), "rate_limited");
}

#[test]
fn execution_lease_survives_releasing_state_lock_and_rejects_another_runner() {
    let workspace = crate::temp_workspace::TempWorkspace::create("forvo-execution-lease").unwrap();
    let store = crate::AssetStore::open_with_policy(
        crate::StoreOptions::new(workspace.path().join("store")),
        crate::pronunciation::PronunciationDomainPolicy,
    )
    .unwrap();
    let mut first = crate::batch_runtime::SafeBatchRuntime::open(store.root(), "lease").unwrap();
    let lease = first.execution_lease().unwrap();
    first.release_lock().unwrap();
    let second = crate::batch_runtime::SafeBatchRuntime::open(store.root(), "lease").unwrap();
    assert!(second.execution_lease().is_err());
    drop(lease);
    assert!(second.execution_lease().is_ok());
}

fn candidate(
    id: &str,
    votes: Option<i64>,
    gender: Option<Gender>,
    country: Option<&str>,
) -> Candidate {
    Candidate {
        record_id: id.into(),
        username: None,
        votes,
        gender,
        country: country.map(str::to_owned),
        download_available: true,
    }
}
#[test]
fn ranking_preserves_negative_unknown_and_male_fallback() {
    let mut candidates = vec![
        candidate("10", None, Some(Gender::Female), Some("Japan")),
        candidate("9", Some(-1), Some(Gender::Male), None),
        candidate("8", Some(0), Some(Gender::Female), None),
        candidate("7", Some(0), Some(Gender::Male), Some("Japan")),
        candidate("6", Some(3), Some(Gender::Male), None),
        candidate("5", Some(0), Some(Gender::Female), Some("Japan")),
    ];
    rank(&mut candidates);
    assert_eq!(
        candidates
            .iter()
            .map(|c| c.record_id.as_str())
            .collect::<Vec<_>>(),
        ["6", "5", "8", "7", "9", "10"]
    );
    candidates.reverse();
    rank(&mut candidates);
    assert_eq!(candidates[0].record_id, "6");
    let mut males = vec![
        candidate("3", Some(2), Some(Gender::Male), None),
        candidate("2", Some(2), Some(Gender::Male), None),
    ];
    rank(&mut males);
    assert_eq!(males[0].record_id, "2");
}
#[test]
fn authentication_requires_site_signals_and_http_success() {
    let mut observed = AuthObservation {
        same_origin: true,
        status: 200,
        challenge: false,
        logout: false,
        login: true,
        ready: true,
    };
    assert_eq!(observed.state(), AuthState::LoginRequired);
    observed.logout = true;
    assert_eq!(observed.state(), AuthState::Authenticated); // общие login-ссылки не опровергают меню аккаунта
    observed.challenge = true;
    assert_eq!(observed.state(), AuthState::ChallengeRequired);
    observed.challenge = false;
    observed.status = 403;
    assert_eq!(observed.state(), AuthState::AccessDenied);
    observed.status = 503;
    assert_eq!(observed.state(), AuthState::SiteUnavailable);
    observed.status = 200;
    observed.same_origin = false;
    assert_eq!(observed.state(), AuthState::SiteUnavailable);
    observed.same_origin = true;
    observed.ready = false;
    assert_eq!(observed.state(), AuthState::PageUnstable);
    for code in [
        "challenge_required",
        "login_required",
        "session_expired",
        "rate_limited",
        "dom_changed",
        "browser_recovery_required",
    ] {
        assert!(pauses_batch(&error(code, "восстановите сессию")));
    }
    assert!(!pauses_batch(&error(
        "japanese_pronunciation_not_found",
        "другое слово"
    )));
}
fn begin(guid: &str, id: &str) -> EventDownloadWillBegin {
    EventDownloadWillBegin {
        frame_id: "frame".to_owned().into(),
        guid: guid.into(),
        url: format!("https://forvo.com/download/mp3/%E5%85%83%E6%B0%97/ja/{id}"),
        suggested_filename: "unexpected.exe".into(),
    }
}
fn progress(guid: &str, state: DownloadProgressState) -> EventDownloadProgress {
    EventDownloadProgress {
        guid: guid.into(),
        state,
        total_bytes: 123.0,
        received_bytes: 123.0,
        file_path: None,
    }
}
#[test]
fn download_needs_exact_button_url_guid_and_completed_event() {
    use download::DownloadTracker;
    let mut tracker = DownloadTracker::default();
    tracker
        .progress(&progress("abcd", DownloadProgressState::Completed))
        .unwrap();
    assert!(tracker.completed().unwrap().is_none());
    tracker.begin(&begin("abcd", "101"), "元気", "101").unwrap();
    assert_eq!(tracker.completed().unwrap(), Some(("abcd", 123)));
    assert!(tracker.begin(&begin("abcd", "101"), "元気", "101").is_err());
    for url in [
        "https://forvo.com/download/mp3/%E5%85%83%E6%B0%97",
        "https://forvo.com/download/mp3/%E5%85%83%E6%B0%97/zh/101",
        "https://forvo.com/download/mp3/%E5%85%83%E6%B0%97/ja/102",
        "https://forvo.com.evil/download/mp3/%E5%85%83%E6%B0%97/ja/101",
        "https://forvo.com/download/mp3/%E5%85%83%E6%B0%97/ja/101?token=secret",
    ] {
        assert!(!download::matches_download(url, "元気", "101"));
    }
    let mut tracker = DownloadTracker::default();
    tracker.begin(&begin("abcd", "101"), "元気", "101").unwrap();
    tracker
        .progress(&progress("1234", DownloadProgressState::Completed))
        .unwrap();
    assert_eq!(tracker.completed().unwrap(), None);
    tracker
        .progress(&progress("abcd", DownloadProgressState::Canceled))
        .unwrap();
    assert_eq!(
        provider_code(&tracker.completed().unwrap_err()),
        "download_aborted"
    );
    let mut event = progress("ffff", DownloadProgressState::InProgress);
    event.received_bytes = f64::NAN;
    assert!(tracker.progress(&event).is_err());
    event.received_bytes = crate::pronunciation::MAX_AUDIO_BYTES as f64 + 1.0;
    assert!(tracker.progress(&event).is_err());
}
pub(crate) fn evidence(bytes: &[u8], request: &Pronunciation) -> ForvoEvidence {
    ForvoEvidence {
        schema_version: 1,
        page_url: word_url(&request.surface),
        downloaded_sha256: crate::hashing::sha256_hex(bytes),
        selected_record_id: "101".into(),
        ranking_policy: RANKING.into(),
        candidates: vec![candidate(
            "101",
            Some(1),
            Some(Gender::Female),
            Some("Japan"),
        )],
        attempts: vec![Attempt {
            record_id: "101".into(),
            code: "downloaded".into(),
        }],
    }
}
#[test]
fn evidence_binds_surface_sha_selected_record_and_order() {
    let request = Pronunciation::new("元気", "げんき").unwrap();
    let e = evidence(b"bytes", &request);
    e.validate(&request, &e.downloaded_sha256).unwrap();
    let mut bad = e.clone();
    bad.selected_record_id = "102".into();
    assert!(bad.validate(&request, &e.downloaded_sha256).is_err());
    bad = e.clone();
    bad.page_url.push_str("?auth=secret");
    assert!(bad.validate(&request, &e.downloaded_sha256).is_err());
    assert!(
        e.validate(
            &Pronunciation::new("幽霊", "ゆうれい").unwrap(),
            &e.downloaded_sha256
        )
        .is_err()
    );
    assert!(e.validate(&request, &"0".repeat(64)).is_err());
    bad = e.clone();
    bad.attempts.clear();
    assert!(bad.validate(&request, &e.downloaded_sha256).is_err());
}

/// Offline Chromium: выполняет тот же адаптер над минимизированным наблюдённым DOM.
#[tokio::test]
#[ignore = "нужен установленный Chromium; внешняя сеть не используется"]
async fn browser_multilingual_dom_and_view_all_are_structural() {
    let browser = BrowserSession::launch(BrowserRuntimeConfig::default())
        .await
        .unwrap();
    browser
        .page()
        .set_content(include_str!("fixtures/multilingual.html"))
        .await
        .unwrap();
    async fn scan(page: &chromiumoxide::Page) -> candidates::DomResult {
        page.evaluate_expression(format!("((location)=>{}('元気','expand',''))({{origin:'https://forvo.com',pathname:'/word/%E5%85%83%E6%B0%97/'}})",include_str!("dom.js"))).await.unwrap().into_value().unwrap()
    }
    let result = scan(browser.page()).await;
    assert_eq!(result.code, "ok");
    assert_eq!(result.candidates.len(), 3);
    assert_eq!(result.candidates[0].votes, Some(-1));
    assert_eq!(result.candidates[1].votes, Some(0));
    assert_eq!(result.candidates[2].votes, None);
    assert_eq!(result.candidates[2].gender, None);
    assert!(
        browser
            .page()
            .evaluate_expression(
                "document.getElementById('show-more-pronunciations-ja').dataset.expanded==='yes'"
            )
            .await
            .unwrap()
            .into_value::<bool>()
            .unwrap()
    );
    browser
        .page()
        .evaluate_expression("document.querySelector('#language-container-ja').remove()")
        .await
        .unwrap();
    assert_eq!(
        scan(browser.page()).await.code,
        "japanese_pronunciation_not_found"
    );
    browser
        .page()
        .set_content(include_str!("fixtures/multilingual.html").replace(
            "data-p3=\"ja\" data-p4=\"101\"",
            "data-p3=\"zh\" data-p4=\"101\"",
        ))
        .await
        .unwrap();
    assert_eq!(scan(browser.page()).await.code, "page_identity_mismatch");
    browser
        .page()
        .set_content(
            include_str!("fixtures/multilingual.html").replace("/languages/ja/", "/languages/zh/"),
        )
        .await
        .unwrap();
    assert_eq!(scan(browser.page()).await.code, "dom_changed");
    browser.close().await.unwrap();
}
