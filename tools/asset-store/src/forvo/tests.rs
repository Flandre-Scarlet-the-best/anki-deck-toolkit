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
async fn cancelled_shutdown_future_is_awaited_by_session_close() {
    let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let marker = completed.clone();
    let task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        marker.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    });
    let mut session = ForvoSession {
        browser: None,
        pending_stop: Some(task),
        pending_launch: None,
        was_authenticated: false,
        download_used: true,
        profile: "/unused".into(),
        visible: false,
    };
    assert!(
        tokio::time::timeout(Duration::from_millis(5), session.stop_browser())
            .await
            .is_err()
    );
    session.close().await.unwrap();
    assert!(completed.load(std::sync::atomic::Ordering::SeqCst));
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

#[tokio::test(flavor = "current_thread")]
async fn cancellation_reloads_revision_without_rehashing_unchanged_blobs() {
    use super::batch::{FetchItem, FetchPlan, StoredAcquisition};
    use crate::batch_runtime::{REFERENCED_BLOB_READS, SafeBatchRuntime};
    let workspace = crate::temp_workspace::TempWorkspace::create("forvo-cancel-poll").unwrap();
    let store = crate::AssetStore::open_with_policy(
        crate::StoreOptions::new(workspace.path().join("store")),
        crate::pronunciation::PronunciationDomainPolicy,
    )
    .unwrap();
    let items = ["あ", "い", "う", "え"]
        .into_iter()
        .map(|word| FetchItem {
            surface: word.into(),
            reading: word.into(),
            replace_expected_sha256: None,
        })
        .collect();
    batch::start(
        &store,
        "poll",
        FetchPlan {
            schema_version: 1,
            items,
        },
    )
    .unwrap();
    let mut runtime = SafeBatchRuntime::open(store.root(), "poll").unwrap();
    let mut state: batch::ForvoBatch = runtime.load().unwrap().unwrap();
    for (index, item) in state.items.iter_mut().enumerate() {
        let bytes = vec![index as u8; 2 * 1024 * 1024];
        let blob = runtime.persist_blob(&bytes, "mp3").unwrap();
        let c = candidate("1", Some(1), Some(Gender::Female), None);
        item.acquisition = Some(StoredAcquisition {
            evidence: ForvoEvidence {
                schema_version: 1,
                page_url: word_url(&item.request.surface),
                downloaded_sha256: blob.sha256.clone(),
                selected_record_id: "1".into(),
                ranking_policy: RANKING.into(),
                candidates: vec![c],
                attempts: vec![Attempt {
                    record_id: "1".into(),
                    code: "downloaded".into(),
                }],
            },
            blob,
        });
    }
    state.revision += 1;
    runtime.save(&state).unwrap();
    drop(runtime);
    // Диагностика прежней границы: четыре полных load того же пакета.
    let mut baseline = SafeBatchRuntime::open(store.root(), "poll").unwrap();
    REFERENCED_BLOB_READS.with(|reads| reads.set(0));
    for _ in 0..4 {
        baseline.load::<batch::ForvoBatch>().unwrap().unwrap();
        baseline.release_lock().unwrap();
        baseline.reacquire_lock().unwrap();
    }
    REFERENCED_BLOB_READS.with(|reads| assert_eq!(reads.get(), 16));
    drop(baseline);
    REFERENCED_BLOB_READS.with(|reads| reads.set(0));
    let cancelled_at = std::cell::Cell::new(None);
    let (observed, ()) = tokio::join!(batch::cancellation(&store, "poll"), async {
        tokio::time::sleep(Duration::from_millis(850)).await;
        batch::cancel(&store, "poll").unwrap();
        cancelled_at.set(Some(std::time::Instant::now()));
    });
    observed.unwrap();
    assert!(cancelled_at.get().unwrap().elapsed() < Duration::from_secs(5));
    // По четыре blob при первоначальной загрузке наблюдателя и команде cancel.
    REFERENCED_BLOB_READS.with(|reads| assert_eq!(reads.get(), 8));
}

#[test]
fn resume_reload_after_acquisition_reuses_verified_blob_cache() {
    use super::batch::{FetchItem, FetchPlan, StoredAcquisition};
    use crate::batch_runtime::{REFERENCED_BLOB_READS, SafeBatchRuntime};

    let workspace = crate::temp_workspace::TempWorkspace::create("forvo-resume-reload").unwrap();
    let store = crate::AssetStore::open_with_policy(
        crate::StoreOptions::new(workspace.path().join("store")),
        crate::pronunciation::PronunciationDomainPolicy,
    )
    .unwrap();
    batch::start(
        &store,
        "resume-cache",
        FetchPlan {
            schema_version: 1,
            items: vec![FetchItem {
                surface: "あ".into(),
                reading: "あ".into(),
                replace_expected_sha256: None,
            }],
        },
    )
    .unwrap();

    let mut runtime = SafeBatchRuntime::open(store.root(), "resume-cache").unwrap();
    let mut state: batch::ForvoBatch = runtime.load().unwrap().unwrap();
    let bytes = vec![0x5a; 1024];
    let blob = runtime.persist_blob(&bytes, "mp3").unwrap();
    let item_evidence = evidence(&bytes, &state.items[0].request);
    state.items[0].acquisition = Some(StoredAcquisition {
        evidence: item_evidence,
        blob,
    });
    state.revision += 1;
    runtime.save(&state).unwrap();

    REFERENCED_BLOB_READS.with(|reads| reads.set(0));
    runtime.release_lock().unwrap();
    runtime.reacquire_lock().unwrap();
    let resumed = batch::reload_after_acquisition(&mut runtime).unwrap();

    assert_eq!(resumed.revision, state.revision);
    assert!(resumed.items[0].acquisition.is_some());
    REFERENCED_BLOB_READS.with(|reads| assert_eq!(reads.get(), 0));
}

#[tokio::test]
async fn invalid_mp3_and_cas_conflict_are_terminal_after_checkpoint() {
    use super::batch::{FetchItem, FetchPlan, StoredAcquisition};
    use crate::batch_runtime::SafeBatchRuntime;
    let workspace = crate::temp_workspace::TempWorkspace::create("forvo-terminal-import").unwrap();
    let store = crate::AssetStore::open_with_policy(
        crate::StoreOptions::new(workspace.path().join("store")),
        crate::pronunciation::PronunciationDomainPolicy,
    )
    .unwrap();
    let existing = Pronunciation::new("え", "え").unwrap();
    let valid = synthetic_mp3();
    crate::pronunciation::import_bytes(&store, &existing, &valid, "fixture.mp3", None).unwrap();
    batch::start(
        &store,
        "terminal",
        FetchPlan {
            schema_version: 1,
            items: vec![
                FetchItem {
                    surface: "あ".into(),
                    reading: "あ".into(),
                    replace_expected_sha256: None,
                },
                FetchItem {
                    surface: "え".into(),
                    reading: "え".into(),
                    replace_expected_sha256: Some("0".repeat(64)),
                },
            ],
        },
    )
    .unwrap();
    let mut runtime = SafeBatchRuntime::open(store.root(), "terminal").unwrap();
    let mut state: batch::ForvoBatch = runtime.load().unwrap().unwrap();
    for (item, bytes) in state
        .items
        .iter_mut()
        .zip([b"broken mp3".as_slice(), valid.as_slice()])
    {
        let blob = runtime.persist_blob(bytes, "mp3").unwrap();
        item.acquisition = Some(StoredAcquisition {
            evidence: evidence(bytes, &item.request),
            blob,
        });
    }
    state.revision += 1;
    runtime.save(&state).unwrap();
    drop(runtime);
    let result = batch::resume(
        &store,
        "terminal",
        &workspace.path().join("never-created"),
        10,
    )
    .await
    .unwrap();
    assert!(result.paused.is_none());
    assert_eq!(
        result.items[0].outcome.as_ref().unwrap().code,
        "validator_failure"
    );
    assert_eq!(
        result.items[1].outcome.as_ref().unwrap().code,
        "identity_conflict"
    );
    assert!(!workspace.path().join("never-created").exists());
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

/// Chromium без внешней сети: выполняет тот же адаптер над минимизированным наблюдённым DOM.
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

#[tokio::test]
#[ignore = "нужен установленный Chromium; внешняя сеть не используется"]
async fn browser_persistent_background_reuses_cookies_and_removes_staging() {
    use chromiumoxide::cdp::browser_protocol::network::{CookieParam, TimeSinceEpoch};
    let workspace =
        crate::temp_workspace::TempWorkspace::create("forvo-persistent-browser-test").unwrap();
    let profile = workspace.path().join("profile");
    let browser =
        BrowserSession::launch_persistent_background(BrowserRuntimeConfig::default(), &profile)
            .await
            .unwrap();
    let staging = browser.download_staging().unwrap();
    std::fs::write(staging.join("unfinished-guid"), b"partial").unwrap();
    let cookie = CookieParam::builder()
        .name("offline_test")
        .value("persistent")
        .url("https://example.test/")
        .expires(TimeSinceEpoch::new(4_000_000_000_f64))
        .build()
        .unwrap();
    browser.browser().set_cookies(vec![cookie]).await.unwrap();
    assert!(
        BrowserSession::launch_persistent_background(BrowserRuntimeConfig::default(), &profile)
            .await
            .is_err()
    );
    browser.close().await.unwrap();
    assert!(!staging.exists());
    let second =
        BrowserSession::launch_persistent_background(BrowserRuntimeConfig::default(), &profile)
            .await
            .unwrap();
    let cookies = second.browser().get_cookies().await.unwrap();
    assert!(
        cookies
            .iter()
            .any(|c| c.name == "offline_test" && c.value == "persistent")
    );
    second.close().await.unwrap();
    workspace.close().unwrap();
}

#[tokio::test]
#[ignore = "нужен установленный Chromium; внешняя сеть не используется"]
async fn browser_cancelled_relaunch_is_joined_before_profile_reuse() {
    let workspace = crate::temp_workspace::TempWorkspace::create("forvo-cancel-relaunch").unwrap();
    let profile = workspace.path().join("profile");
    let mut session = ForvoSession::launch(&profile).await.unwrap();
    session.stop_browser().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(5), session.start_browser(false))
            .await
            .is_err()
    );
    assert!(session.pending_launch.is_some());
    session.close().await.unwrap();
    let reopened = ForvoSession::launch(&profile).await.unwrap();
    reopened.close().await.unwrap();
    workspace.close().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn successful_acquisition_and_import_keep_two_independent_full_checks() {
    let bytes = synthetic_mp3();
    let mut source = source(vec![Ok(bytes.clone())]);
    let request = Pronunciation::new("こいつ", "こいつ").unwrap();
    let workspace = crate::temp_workspace::TempWorkspace::create("forvo-full-checks").unwrap();
    let store = crate::AssetStore::open_with_policy(
        crate::StoreOptions::new(workspace.path().join("store")),
        crate::pronunciation::PronunciationDomainPolicy,
    )
    .unwrap();
    crate::pronunciation::FULL_PROBES.with(|count| count.set(0));
    let acquired = evidence::acquire_from(&mut source, &request).await.unwrap();
    crate::pronunciation::FULL_PROBES.with(|count| assert_eq!(count.get(), 1));
    let record = import_acquisition(&store, &request, &acquired, None).unwrap();
    crate::pronunciation::FULL_PROBES.with(|count| assert_eq!(count.get(), 2));
    assert_eq!(record.sha256, crate::hashing::sha256_hex(&bytes));
    assert_eq!(
        record.current_validation_status(),
        Some(crate::SemanticStatus::Uncertain)
    );
    assert!(record.human_attestation.is_none());
    let validation = record.validation.as_ref().unwrap();
    assert_eq!(
        validation.validator,
        crate::pronunciation::technical_validator_identity()
    );
    assert_eq!(
        validation.evidence[0].details.as_ref().unwrap()["probe"]["sha256"],
        record.sha256
    );
    assert!(crate::pronunciation::read_trusted(store.root(), &[request]).is_err());
}

#[tokio::test]
async fn download_infrastructure_pauses_unfinished_item_and_can_resume_after_repair() {
    use crate::batch_runtime::SafeBatchRuntime;
    for code in [
        "download_staging_failed",
        "download_listener_failed",
        "download_not_supported",
        "download_file_missing",
        "download_read_failed",
        "browser_recovery_required",
        "candidate_changed",
        "candidate_disappeared",
        "download_timeout",
        "technical_validation_failed",
    ] {
        let workspace =
            crate::temp_workspace::TempWorkspace::create("forvo-download-recovery").unwrap();
        let store = crate::AssetStore::open_with_policy(
            crate::StoreOptions::new(workspace.path().join("store")),
            crate::pronunciation::PronunciationDomainPolicy,
        )
        .unwrap();
        batch::start(
            &store,
            "recovery",
            batch::FetchPlan {
                schema_version: 1,
                items: vec![batch::FetchItem {
                    surface: "あ".into(),
                    reading: "あ".into(),
                    replace_expected_sha256: None,
                }],
            },
        )
        .unwrap();
        let mut source = source(vec![Err(error(
            code,
            "Исправьте окружение и повторите batch-resume",
        ))]);
        let mut runtime = SafeBatchRuntime::open(store.root(), "recovery").unwrap();
        let mut state: batch::ForvoBatch = runtime.load().unwrap().unwrap();
        let result = evidence::acquire_from(&mut source, &state.items[0].request).await;
        assert_eq!(source.calls, ["1"], "{code}");
        assert!(matches!(
            batch::checkpoint_acquisition(&mut runtime, &mut state, 0, result).unwrap(),
            batch::AcquisitionProgress::Paused
        ));
        drop(runtime);
        let paused = batch::status(&store, "recovery").unwrap();
        let failure = paused.paused.unwrap();
        assert_eq!(failure.code, code);
        assert!(failure.message.contains("batch-resume"));
        assert_eq!(failure.report.unwrap().attempts[0].code, code);
        assert!(paused.items[0].outcome.is_none());
        assert!(paused.items[0].acquisition.is_none());
        // После ремонта окружения получаем исходные байты и сохраняем штатный checkpoint.
        let bytes = synthetic_mp3();
        source.responses.push_back(Ok(bytes.clone()));
        let result = evidence::acquire_from(&mut source, &state.items[0].request).await;
        assert_eq!(source.calls, ["1", "1"]);
        let mut runtime = SafeBatchRuntime::open(store.root(), "recovery").unwrap();
        state = runtime.load().unwrap().unwrap();
        assert!(matches!(
            batch::checkpoint_acquisition(&mut runtime, &mut state, 0, result).unwrap(),
            batch::AcquisitionProgress::Ready
        ));
        drop(runtime);
        let resumed = batch::resume(
            &store,
            "recovery",
            &workspace.path().join("unused-profile"),
            1,
        )
        .await
        .unwrap();
        assert!(resumed.paused.is_none());
        assert_eq!(resumed.items[0].outcome.as_ref().unwrap().code, "imported");
        assert_eq!(
            resumed.items[0].acquisition.as_ref().unwrap().blob.sha256,
            crate::hashing::sha256_hex(&bytes)
        );
        assert!(!workspace.path().join("unused-profile").exists());
    }
}

#[tokio::test]
async fn candidate_failures_exhaust_records_but_unknown_errors_keep_their_code() {
    let request = Pronunciation::new("あ", "あ").unwrap();
    let codes = [
        "download_aborted",
        "download_mismatch",
        "download_invalid",
        "download_incomplete",
        "download_button_unavailable",
    ];
    for code in codes {
        let mut source = source(
            (0..3)
                .map(|_| Err(error(code, "Запись отклонена")))
                .collect(),
        );
        let failure = evidence::acquire_from(&mut source, &request)
            .await
            .err()
            .unwrap();
        assert_eq!(source.calls, ["1", "2", "3"]);
        assert_eq!(provider_code(&failure), "candidates_exhausted");
        assert!(!pauses_batch(&failure));
        assert_eq!(failure.details["attempts"][2]["code"], code);
    }
    let mut source = source(vec![Err(error(
        "unexpected_failure",
        "Отдельная диагностика",
    ))]);
    let failure = evidence::acquire_from(&mut source, &request)
        .await
        .err()
        .unwrap();
    assert_eq!(provider_code(&failure), "unexpected_failure");
    assert_eq!(source.calls, ["1"]);
}
