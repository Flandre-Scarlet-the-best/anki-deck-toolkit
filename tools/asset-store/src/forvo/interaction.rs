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
    use tokio::io::AsyncReadExt;
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

    /// Итог дочернего запуска: код выхода и прочитанный stdout.
    #[derive(Debug)]
    struct SignalChildOutcome {
        status: std::process::ExitStatus,
        stdout: Vec<u8>,
    }

    /// Останавливает дочерний процесс и убирает его, не дожидаясь ответа дольше
    /// `SIGNAL_CHILD_CLEANUP_TIMEOUT`; возвращает описание обеих операций.
    async fn stop_signal_test_child(child: &mut tokio::process::Child) -> String {
        let stop = tokio::time::timeout(SIGNAL_CHILD_CLEANUP_TIMEOUT, child.kill()).await;
        let reap = tokio::time::timeout(SIGNAL_CHILD_CLEANUP_TIMEOUT, child.wait()).await;
        format!("остановка={stop:?}, уборка={reap:?}")
    }

    /// Дожидается дочернего процесса в пределах `timeout` и читает его stdout.
    /// Превышение срока и отказ ожидания не маскируются: процесс сначала
    /// останавливается и убирается, а причина возвращается текстом, поэтому
    /// вызывающий тест не оставляет работающих потомков.
    async fn wait_for_signal_test_child(
        mut child: tokio::process::Child,
        timeout: Duration,
    ) -> Result<SignalChildOutcome, String> {
        let pid = child.id();
        let mut stdout = child
            .stdout
            .take()
            .expect("stdout дочернего процесса подключён к каналу");
        let waited = tokio::time::timeout(timeout, child.wait()).await;
        let status = match waited {
            Ok(Ok(status)) => status,
            Ok(Err(error)) => {
                let cleanup = stop_signal_test_child(&mut child).await;
                return Err(format!(
                    "ожидание дочернего процесса отказало (pid={pid:?}): {error}; {cleanup}"
                ));
            }
            Err(_) => {
                let cleanup = stop_signal_test_child(&mut child).await;
                return Err(format!(
                    "дочерний процесс превысил срок {timeout:?} (pid={pid:?}); {cleanup}"
                ));
            }
        };
        // Единственный писатель в этот поток завершён, поэтому чтение доходит до
        // конца потока; ошибка чтения не подменяется успехом.
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await.map_err(|error| {
            format!("чтение stdout дочернего процесса отказало (pid={pid:?}): {error}")
        })?;
        Ok(SignalChildOutcome {
            status,
            stdout: bytes,
        })
    }

    /// Проверяет итог дочернего запуска: успешный код выхода и контрольный маркер,
    /// который печатается только после всех проверок сигналов. Возвращает текст
    /// отказа вместо паники, чтобы те же проверки проходили и заглушки.
    fn signal_test_child_failure(outcome: &SignalChildOutcome) -> Option<String> {
        let stdout = String::from_utf8_lossy(&outcome.stdout);
        if !outcome.status.success() {
            return Some(format!(
                "дочерний процесс завершился со статусом {}; stdout: {stdout}",
                outcome.status
            ));
        }
        if !stdout.contains(SIGNAL_TEST_SUCCESS_MARKER) {
            return Some(format!(
                "точный фильтр не выполнил проверки сигналов и не оставил маркер \
                 {SIGNAL_TEST_SUCCESS_MARKER}; stdout: {stdout}"
            ));
        }
        None
    }

    /// Заглушка дочернего процесса для проверок обвязки: команда завершается по
    /// правилам своего сценария, поэтому отказ и превышение срока воспроизводимы.
    fn signal_test_stub(script: &str) -> tokio::process::Child {
        tokio::process::Command::new("sh")
            .args(["-c", script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("заглушка дочернего процесса запускается")
    }

    #[tokio::test]
    async fn immediate_signals_are_queued_before_wait_future_is_polled() {
        // SIGTERM, SIGHUP и SIGINT действуют на весь процесс. Контракт проверяется
        // отдельным процессом, чтобы сигналы не прерывали соседние модульные тесты.
        if std::env::var_os(SIGNAL_TEST_CHILD_ENV).is_none() {
            let mut command = tokio::process::Command::new(
                std::env::current_exe().expect("исполняемый файл тестов"),
            );
            // Точный фильтр запускает только контракт сигналов, а `--nocapture`
            // показывает контрольный маркер в stdout дочернего процесса.
            command
                .args(["--exact", SIGNAL_TEST_NAME, "--nocapture"])
                .env(SIGNAL_TEST_CHILD_ENV, "1")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::inherit())
                .kill_on_drop(true);
            let child = command
                .spawn()
                .expect("дочерний процесс теста сигналов должен запуститься");
            let pid = child.id();
            let outcome = wait_for_signal_test_child(child, SIGNAL_TEST_TIMEOUT)
                .await
                .unwrap_or_else(|failure| panic!("{failure}"));
            assert_eq!(
                signal_test_child_failure(&outcome),
                None,
                "дочерний тест сигналов не подтвердил контракт (pid={pid:?})"
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

    /// Зависший дочерний процесс не даёт ложного успеха и не остаётся работать:
    /// обвязка останавливает его и убирает в пределах своего срока.
    #[tokio::test]
    async fn hung_signal_test_child_is_stopped_without_leftover_process() {
        let child = signal_test_stub("exec sleep 30");
        let pid = child.id().expect("у запущенной заглушки есть pid");
        let started = std::time::Instant::now();
        let failure = wait_for_signal_test_child(child, Duration::from_millis(200))
            .await
            .expect_err("зависший дочерний процесс не даёт итога");
        assert!(failure.contains("превысил срок"), "{failure}");
        assert!(started.elapsed() < SIGNAL_TEST_TIMEOUT, "{failure}");
        // Убранный процесс не оставляет записи в /proc: остаться он уже не может.
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        assert!(
            status.is_empty(),
            "дочерний процесс {pid} остался: {status}"
        );
    }

    /// Успешный код выхода без контрольного маркера не принимается за выполнение
    /// проверок, а неуспешный код выхода виден и при оставленном маркере.
    #[tokio::test]
    async fn signal_test_child_outcome_requires_marker_and_success() {
        let quiet = wait_for_signal_test_child(
            signal_test_stub("echo посторонний вывод"),
            SIGNAL_TEST_TIMEOUT,
        )
        .await
        .expect("заглушка завершается");
        let failure = signal_test_child_failure(&quiet).expect("без маркера итог не подтверждён");
        assert!(failure.contains("не оставил маркер"), "{failure}");

        let broken = wait_for_signal_test_child(
            signal_test_stub(&format!("echo {SIGNAL_TEST_SUCCESS_MARKER}; exit 3")),
            SIGNAL_TEST_TIMEOUT,
        )
        .await
        .expect("заглушка завершается");
        assert!(!broken.status.success());
        let failure =
            signal_test_child_failure(&broken).expect("ненулевой код выхода должен быть виден");
        assert!(failure.contains("статусом"), "{failure}");

        let passed = wait_for_signal_test_child(
            signal_test_stub(&format!("echo {SIGNAL_TEST_SUCCESS_MARKER}")),
            SIGNAL_TEST_TIMEOUT,
        )
        .await
        .expect("заглушка завершается");
        assert_eq!(signal_test_child_failure(&passed), None);
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
