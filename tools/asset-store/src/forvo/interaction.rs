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
    #[tokio::test]
    async fn immediate_signals_are_queued_before_wait_future_is_polled() {
        use rustix::process::{Signal, getpid, kill_process};
        for signal in [Signal::TERM, Signal::HUP, Signal::INT] {
            let mut wait = Some(begin(Duration::from_secs(30)).unwrap());
            kill_process(getpid(), signal).unwrap();
            tokio::time::timeout(Duration::from_secs(2), wait_interrupt(&mut wait))
                .await
                .expect("сигнал сразу после события не должен теряться");
            wait.take().unwrap().finish("interrupted");
        }
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
