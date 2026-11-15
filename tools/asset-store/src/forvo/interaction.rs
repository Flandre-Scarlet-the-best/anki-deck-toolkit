//! Событийный канал ручного шага Forvo: NDJSON в stderr, итог команды — в stdout.
use crate::{AssetError, ErrorCode};
use serde_json::json;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn write_event(writer: &mut impl Write, value: &serde_json::Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

pub(crate) struct HumanWait {
    id: String,
    finished: bool,
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}
impl HumanWait {
    pub(crate) fn finish(mut self, outcome: &str) {
        end(&self.id, outcome);
        self.finished = true;
    }
}
impl Drop for HumanWait {
    fn drop(&mut self) {
        if !self.finished {
            end(&self.id, "interrupted");
        }
    }
}

pub(crate) fn begin(wait: Duration) -> Result<HumanWait, AssetError> {
    use tokio::signal::unix::{SignalKind, signal};
    // Подписки существуют до публикации события: немедленный сигнал не теряется.
    let subscribe = || -> std::io::Result<_> {
        Ok((
            signal(SignalKind::interrupt())?,
            signal(SignalKind::terminate())?,
            signal(SignalKind::hangup())?,
        ))
    };
    let (interrupt, terminate, hangup) =
        subscribe().map_err(|e| AssetError::io("сигналы ручного шага Forvo", e))?;
    let id = format!(
        "forvo-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    write_event(&mut std::io::stderr().lock(), &json!({
        "event":"human_interaction_required", "provider":"forvo_web", "session_id":id,
        "action":"authenticate_or_challenge", "wait_mode":"deadline", "timeout_seconds":wait.as_secs(),
        "hint":"Войдите в Forvo или завершите проверку вручную в открытом окне. Пароль не передаётся CLI.",
        "next_action":"Дождитесь проверки авторизации самим CLI; процесс продолжится в той же сессии"
    })).map_err(|e| AssetError::io("событие ручного шага Forvo", e))?;
    Ok(HumanWait {
        id,
        finished: false,
        interrupt,
        terminate,
        hangup,
    })
}

/// Расширение не ждёт внутри CLI: следующий шаг — ручная проверка и явный повтор.
pub(crate) fn extension_required(action: &str, next_action: &str) {
    let id = format!(
        "forvo-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let _ = write_event(
        &mut std::io::stderr().lock(),
        &json!({
            "event":"human_interaction_required", "provider":"forvo_brave", "session_id":id,
            "action":action, "wait_mode":"resume_required", "timeout_seconds":null,
            "hint":"Выполните ручной шаг в выбранном браузере; CLI завершает текущую попытку",
            "next_action":next_action
        }),
    );
}

pub(crate) async fn wait_interrupt(wait: &mut Option<HumanWait>) {
    let Some(wait) = wait else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = wait.interrupt.recv() => (),
        _ = wait.terminate.recv() => (),
        _ = wait.hangup.recv() => (),
    }
}

pub(crate) fn end(id: &str, outcome: &str) {
    let _ = write_event(
        &mut std::io::stderr().lock(),
        &json!({
            "event":"human_interaction_finished", "provider":"forvo_web", "session_id":id, "outcome":outcome
        }),
    );
}

/// Только команды Forvo: штатные сигналы проходят через ту же управляемую очистку.
pub(crate) async fn interrupted() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut hup = signal(SignalKind::hangup())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = term.recv() => Ok(()),
        _ = hup.recv() => Ok(()),
    }
}

pub(crate) fn interrupted_error() -> AssetError {
    AssetError::with_details(
        ErrorCode::Interrupted,
        "Forvo прерван; выполнена управляемая остановка",
        json!({"provider":"forvo_web","code":"interrupted","next_action":"Проверьте итог очистки; продолжите сохранённый пакет через batch-resume"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Writer {
        bytes: Vec<u8>,
        flushed: bool,
    }
    impl Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.flushed = true;
            Ok(())
        }
    }
    #[test]
    fn event_is_one_json_line_and_flushed_before_return() {
        let mut writer = Writer::default();
        write_event(&mut writer, &json!({"event":"human_interaction_required"})).unwrap();
        assert!(writer.flushed);
        assert_eq!(writer.bytes.last(), Some(&b'\n'));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&writer.bytes).unwrap()["event"],
            "human_interaction_required"
        );
    }
    const SIGNAL_TEST_CHILD_ENV: &str = "ASSET_STORE_FORVO_SIGNAL_TEST_CHILD";
    const SIGNAL_TEST_NAME: &str =
        "forvo::interaction::tests::immediate_signals_are_queued_before_wait_future_is_polled";
    const SIGNAL_TEST_SUCCESS_MARKER: &str = "FORVO_SIGNAL_TEST_ASSERTIONS_PASSED";
    const SIGNAL_TEST_TIMEOUT: Duration = Duration::from_secs(10);
    const SIGNAL_CHILD_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);

    async fn cleanup_signal_test_child(child: &mut tokio::process::Child) -> String {
        let kill = tokio::time::timeout(SIGNAL_CHILD_CLEANUP_TIMEOUT, child.kill()).await;
        let wait = tokio::time::timeout(SIGNAL_CHILD_CLEANUP_TIMEOUT, child.wait()).await;
        format!("kill={kill:?}, wait={wait:?}")
    }

    async fn wait_for_signal_test_child(
        child: &mut tokio::process::Child,
    ) -> Result<std::process::ExitStatus, String> {
        let pid = child.id();
        match tokio::time::timeout(SIGNAL_TEST_TIMEOUT, child.wait()).await {
            Ok(Ok(status)) => Ok(status),
            Ok(Err(error)) => {
                let cleanup = cleanup_signal_test_child(child).await;
                Err(format!(
                    "не удалось дождаться дочернего теста сигналов (pid={pid:?}): {error}; очистка: {cleanup}"
                ))
            }
            Err(_) => {
                let cleanup = cleanup_signal_test_child(child).await;
                Err(format!(
                    "дочерний тест сигналов превысил срок {:?} (pid={pid:?}); очистка: {cleanup}",
                    SIGNAL_TEST_TIMEOUT
                ))
            }
        }
    }

    #[tokio::test]
    async fn immediate_signals_are_queued_before_wait_future_is_polled() {
        // SIGTERM/SIGHUP/SIGINT — сигналы всего процесса. Выполняем этот
        // контрактный тест отдельно, чтобы не прерывать соседние unit-тесты.
        if std::env::var_os(SIGNAL_TEST_CHILD_ENV).is_none() {
            let mut child = tokio::process::Command::new(
                std::env::current_exe().expect("исполняемый файл тестов"),
            )
            .args(["--exact", SIGNAL_TEST_NAME])
            .env(SIGNAL_TEST_CHILD_ENV, "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            // Обработчик тестов должен показать маркер в stdout.
            .arg("--nocapture")
            .kill_on_drop(true)
            .spawn()
            .expect("дочерний процесс теста сигналов должен запуститься");
            let pid = child.id();
            let wait_result = wait_for_signal_test_child(&mut child).await;
            let output = tokio::time::timeout(
                SIGNAL_CHILD_CLEANUP_TIMEOUT,
                child.wait_with_output(),
            )
            .await
            .unwrap_or_else(|_| {
                panic!("чтение вывода дочернего теста зависло (pid={pid:?}); ожидание: {wait_result:?}")
            })
            .unwrap_or_else(|error| {
                panic!("не удалось прочитать вывод дочернего теста (pid={pid:?}); ожидание: {wait_result:?}: {error}")
            });
            let status = wait_result.unwrap_or_else(|error| {
                panic!(
                    "{error}; stdout: {}",
                    String::from_utf8_lossy(&output.stdout)
                )
            });
            assert!(
                status.success() && output.status.success(),
                "дочерний тест сигналов завершился со статусом {status} (stdout: {}, pid={pid:?}); см. stderr",
                String::from_utf8_lossy(&output.stdout)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains(SIGNAL_TEST_SUCCESS_MARKER),
                "точный фильтр не выполнил signal assertions (pid={pid:?}); stdout: {}",
                String::from_utf8_lossy(&output.stdout)
            );
            return;
        }

        use rustix::process::{Signal, getpid, kill_process};
        for signal in [Signal::TERM, Signal::HUP, Signal::INT] {
            let mut wait = Some(begin(Duration::from_secs(30)).unwrap());
            kill_process(getpid(), signal).unwrap();
            tokio::time::timeout(Duration::from_secs(2), wait_interrupt(&mut wait))
                .await
                .expect("сигнал сразу после события не должен теряться");
            wait.take().unwrap().finish("interrupted");
        }
        println!("{SIGNAL_TEST_SUCCESS_MARKER}");
    }

    #[tokio::test]
    async fn noninteractive_wait_has_no_spurious_interrupt() {
        assert!(
            tokio::time::timeout(Duration::from_millis(1), wait_interrupt(&mut None))
                .await
                .is_err()
        );
    }
}
