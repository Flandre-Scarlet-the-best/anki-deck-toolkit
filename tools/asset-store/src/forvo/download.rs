use super::{Candidate, ForvoSession, error};
use crate::{
    AssetError,
    pronunciation::{MAX_AUDIO_BYTES, Pronunciation},
    temp_workspace::TempWorkspace,
};
use chromiumoxide::cdp::browser_protocol::browser::{
    CancelDownloadParams, DownloadProgressState, EventDownloadProgress, EventDownloadWillBegin,
    SetDownloadBehaviorBehavior, SetDownloadBehaviorParams,
};
use futures::StreamExt;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::time::Duration;

fn valid_guid(guid: &str) -> bool {
    !guid.is_empty() && guid.len() <= 64 && guid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

pub(crate) fn matches_download(url: &str, surface: &str, id: &str) -> bool {
    let Ok(url) = url::Url::parse(url) else {
        return false;
    };
    let Some(parts) = url.path_segments() else {
        return false;
    };
    let parts = parts.filter(|p| !p.is_empty()).collect::<Vec<_>>();
    let mut expected = url::Url::parse("https://forvo.com/").expect("URL");
    expected
        .path_segments_mut()
        .expect("URL")
        .extend(["download", "mp3", surface, "ja", id]);
    url.origin() == expected.origin()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && parts == expected.path_segments().expect("URL").collect::<Vec<_>>()
}

#[derive(Default)]
pub(crate) struct DownloadTracker {
    begin: Option<String>,
    progress: BTreeMap<String, EventDownloadProgress>,
}
impl DownloadTracker {
    pub fn begin(
        &mut self,
        event: &EventDownloadWillBegin,
        surface: &str,
        id: &str,
    ) -> Result<(), AssetError> {
        if self.begin.is_some()
            || !valid_guid(&event.guid)
            || !matches_download(&event.url, surface, id)
        {
            return Err(error(
                "download_mismatch",
                "Событие загрузки не связано однозначно с выбранной кнопкой; файл отклонён",
            ));
        }
        self.begin = Some(event.guid.clone());
        Ok(())
    }
    pub fn progress(&mut self, event: &EventDownloadProgress) -> Result<(), AssetError> {
        if !valid_guid(&event.guid)
            || self.progress.len() > 8
            || !event.received_bytes.is_finite()
            || !event.total_bytes.is_finite()
            || event.received_bytes < 0.0
            || event.total_bytes < 0.0
            || event.received_bytes > MAX_AUDIO_BYTES as f64
            || event.total_bytes > MAX_AUDIO_BYTES as f64
        {
            return Err(error(
                "download_invalid",
                "Недопустимый размер/состояние загрузки",
            ));
        }
        if self
            .progress
            .get(&event.guid)
            .is_some_and(|old| old.state != DownloadProgressState::InProgress && old != event)
        {
            return Err(error(
                "download_mismatch",
                "Противоречивые финальные события загрузки",
            ));
        }
        self.progress.insert(event.guid.clone(), event.clone());
        Ok(())
    }
    pub fn completed(&self) -> Result<Option<(&str, u64)>, AssetError> {
        let Some(guid) = self.begin.as_deref() else {
            return Ok(None);
        };
        let Some(event) = self.progress.get(guid) else {
            return Ok(None);
        };
        match event.state {
            DownloadProgressState::Canceled => Err(error(
                "download_aborted",
                "Chromium отменил загрузку; проверьте разрешение загрузок и повторите",
            )),
            DownloadProgressState::Completed => {
                if event.received_bytes == 0.0
                    || event.received_bytes.fract() != 0.0
                    || (event.total_bytes > 0.0 && event.total_bytes != event.received_bytes)
                {
                    return Err(error(
                        "download_incomplete",
                        "Размер завершённой загрузки противоречив",
                    ));
                }
                Ok(Some((guid, event.received_bytes as u64)))
            }
            _ => Ok(None),
        }
    }
}

impl ForvoSession {
    pub(crate) async fn download(
        &mut self,
        request: &Pronunciation,
        candidate: &Candidate,
    ) -> Result<Vec<u8>, AssetError> {
        if self.browser.page().url().await.ok().flatten().as_deref()
            != Some(super::word_url(&request.surface).as_str())
        {
            self.navigate(&super::word_url(&request.surface)).await?;
        }
        self.require_authenticated().await?;
        eprintln!("forvo_download_started record_id={}", candidate.record_id);
        let workspace = TempWorkspace::create("forvo-download").map_err(|_| {
            error(
                "download_staging_failed",
                "Не удалось создать приватный staging",
            )
        })?;
        let download_path = workspace.path().to_path_buf();
        self.download_workspaces.push(workspace);
        let browser = self.browser.browser();
        // Слушатели устанавливаются до клика. GUID определяет имя независимо от suggestedFilename/filePath.
        let mut begins = browser
            .event_listener::<EventDownloadWillBegin>()
            .await
            .map_err(|_| {
                error(
                    "download_listener_failed",
                    "CDP не предоставляет события загрузки",
                )
            })?;
        let mut progresses = browser
            .event_listener::<EventDownloadProgress>()
            .await
            .map_err(|_| {
                error(
                    "download_listener_failed",
                    "CDP не предоставляет прогресс загрузки",
                )
            })?;
        browser
            .execute(
                SetDownloadBehaviorParams::builder()
                    .behavior(SetDownloadBehaviorBehavior::AllowAndName)
                    .download_path(download_path.to_string_lossy().to_string())
                    .events_enabled(true)
                    .build()
                    .expect("CDP"),
            )
            .await
            .map_err(|_| {
                error(
                    "download_not_supported",
                    "Chromium не разрешил управляемую загрузку",
                )
            })?;
        let baseline = browser
            .pages()
            .await
            .map_err(|_| {
                error(
                    "browser_recovery_required",
                    "Не удалось зафиксировать вкладки перед кликом",
                )
            })?
            .into_iter()
            .map(|p| p.target_id().clone())
            .collect::<std::collections::HashSet<_>>();
        let mut tracker = DownloadTracker::default();
        let result = async {
            let before = self.dom(&request.surface, "inspect", "").await?;
            if before.code != "ok" || !before.candidates.contains(candidate) {
                return Err(error(
                    "candidate_changed",
                    "Строка кандидата изменилась; повторите выбор",
                ));
            }
            let clicked = self
                .dom(&request.surface, "click", &candidate.record_id)
                .await?;
            if clicked.code != "prepared" {
                return Err(error(
                    &clicked.code,
                    "Не удалось подготовить обычную кнопку выбранной записи; проверьте окно и перекрывающие баннеры",
                ));
            }
            let point = clicked
                .point
                .filter(|p| p.x.is_finite() && p.y.is_finite())
                .ok_or_else(|| error("dom_changed", "Нет координат кнопки"))?;
            let point = chromiumoxide::layout::Point::new(point.x, point.y);
            self.browser.page().move_mouse(point).await.map_err(|_| {
                error(
                    "download_button_unavailable",
                    "Не удалось навести указатель на Download MP3",
                )
            })?;
            self.browser.page().click(point).await.map_err(|_| {
                error(
                    "download_button_unavailable",
                    "Не удалось нажать Download MP3",
                )
            })?;
            eprintln!("forvo_download_clicked record_id={}", candidate.record_id);
            loop {
                if let Some((guid, length)) = tracker.completed()? {
                    let file = OpenOptions::new()
                        .read(true)
                        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
                        .open(download_path.join(guid))
                        .map_err(|_| {
                            error(
                                "download_file_missing",
                                "CDP завершил загрузку, но GUID-файл не доступен",
                            )
                        })?;
                    let metadata = file
                        .metadata()
                        .map_err(|_| error("download_file_missing", "Не удалось проверить файл"))?;
                    if !metadata.is_file() || metadata.len() != length {
                        return Err(error(
                            "download_incomplete",
                            "Размер GUID-файла не совпадает с завершённым событием",
                        ));
                    }
                    let mut bytes = Vec::new();
                    file.take(MAX_AUDIO_BYTES + 1)
                        .read_to_end(&mut bytes)
                        .map_err(|_| {
                            error(
                                "download_read_failed",
                                "Не удалось прочитать завершённую загрузку",
                            )
                        })?;
                    if bytes.len() as u64 != length {
                        return Err(error(
                            "download_incomplete",
                            "Файл изменился во время чтения",
                        ));
                    }
                    return Ok(bytes);
                }
                tokio::select! {
                    begin=begins.next()=>{
                        let begin=begin.ok_or_else(||error("browser_recovery_required","Поток загрузок Chromium закрыт"))?;
                        eprintln!("forvo_download_begin record_id={}",candidate.record_id);
                        let mut owned_frame=self.browser.page().mainframe().await.ok().flatten().as_ref()==Some(&begin.frame_id);
                        if !owned_frame {
                            for page in browser.pages().await.map_err(|_|error("browser_recovery_required","Не удалось сопоставить вкладку загрузки"))? {
                                if !baseline.contains(page.target_id()) && page.mainframe().await.ok().flatten().as_ref()==Some(&begin.frame_id) {owned_frame=true;break;}
                            }
                        }
                        if !owned_frame {let _=browser.execute(CancelDownloadParams::new(begin.guid.clone())).await;return Err(error("download_mismatch","Загрузка начата чужой вкладкой; файл отклонён"));}
                        if let Err(e)=tracker.begin(&begin,&request.surface,&candidate.record_id) {let _=browser.execute(CancelDownloadParams::new(begin.guid.clone())).await;return Err(e);}
                    },
                    progress=progresses.next()=>{
                        let progress=progress.ok_or_else(||error("browser_recovery_required","Поток прогресса Chromium закрыт"))?;
                        if progress.state!=DownloadProgressState::InProgress {eprintln!("forvo_download_terminal state={}",progress.state.as_ref());}
                        tracker.progress(&progress)?;
                    },
                }
            }
        };
        let result = match tokio::time::timeout(Duration::from_secs(120), result).await {
            Ok(r) => r,
            Err(_) => {
                // Проверка может заменить основную страницу либо появиться в новой
                // вкладке. Блокировка не превращается в отсутствие кандидата.
                let mut failure = error(
                    "download_timeout",
                    "Загрузка не завершена; проверьте окно Forvo и продолжите после восстановления",
                );
                match super::observe_auth(self.browser.page()).await {
                    Ok(super::AuthState::Authenticated) => (),
                    Ok(super::AuthState::LoginRequired) if self.was_authenticated => {
                        failure = super::auth_error(super::AuthState::SessionExpired)
                    }
                    Ok(state) => failure = super::auth_error(state),
                    Err(e) => failure = e,
                }
                if let Ok(pages) = self.browser.browser().pages().await {
                    for page in pages {
                        if !baseline.contains(page.target_id()) {
                            match super::observe_auth(&page).await {
                                Ok(state) if state != super::AuthState::Authenticated => {
                                    failure = super::auth_error(state);
                                    break;
                                }
                                Err(e) => {
                                    failure = e;
                                    break;
                                }
                                _ => (),
                            }
                        }
                    }
                }
                Err(failure)
            }
        };
        // Даже при ошибке запрещаем следующие загрузки и отменяем наблюдавшиеся незавершённые.
        let _ = browser
            .execute(SetDownloadBehaviorParams::new(
                SetDownloadBehaviorBehavior::Deny,
            ))
            .await;
        for guid in tracker.progress.keys().chain(tracker.begin.iter()) {
            let _ = browser
                .execute(CancelDownloadParams::new(guid.clone()))
                .await;
        }
        while let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(1), begins.next()).await
        {
            let _ = browser
                .execute(CancelDownloadParams::new(event.guid.clone()))
                .await;
        }
        if let Ok(pages) = browser.pages().await {
            for page in pages {
                if !baseline.contains(page.target_id()) {
                    let _ = page.close().await;
                }
            }
        }
        // Staging принадлежит сессии до доказанной остановки Chromium, в том числе при Ctrl+C.
        result
    }
}
