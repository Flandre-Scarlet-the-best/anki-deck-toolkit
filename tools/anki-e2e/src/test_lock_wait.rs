//! Ожидание освобождения `flock`, чьё открытое описание могло быть унаследовано.
//!
//! Linux связывает `flock` с открытым описанием файла, а не с дескриптором
//! (flock(2), open(2)): `O_CLOEXEC` закрывает унаследованную копию только на
//! `exec`. Поэтому дочерний процесс, форкнутый параллельным тестом того же
//! процесса, кратковременно удерживает то же описание уже после `drop`
//! законного владельца, и немедленный повторный неблокирующий захват получает
//! `EWOULDBLOCK`, хотя контракт «владелец отпустил — замок свободен» выполнен.
//!
//! Помощник ждёт только ту ошибку, которую тест сам назвал временно занятым
//! замком, и только ограниченное время: результат, не связанный с занятым
//! замком, возвращается без изменений. Модуль компилируется только в
//! режиме тестирования, поэтому политика немедленного отказа в рабочем коде
//! при настоящем владельце не меняется.

use std::fmt;
use std::time::{Duration, Instant};

/// Сколько тест ждёт освобождения замка, когда держатель уже отпущен.
pub(crate) const RELEASE_LIMIT: Duration = Duration::from_secs(5);

/// Пауза между попытками: окно `fork`–`exec` много короче, а занятость дольше
/// [`RELEASE_LIMIT`] ожидание всё равно не продлевает.
const RETRY_INTERVAL: Duration = Duration::from_millis(2);

/// Почему ожидание прекращено.
#[derive(Debug)]
pub(crate) enum LockWaitError<E> {
    /// Результат не означает временно занятый замок: возвращается без изменений.
    Other(E),
    /// Замок оставался занят всё отведённое время.
    Timeout { last: E, waited: Duration },
}

impl<E: fmt::Display> fmt::Display for LockWaitError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Other(error) => write!(f, "{error}"),
            Self::Timeout { last, waited } => write!(
                f,
                "замок не освободился за {waited:?}; последний отказ: {last}"
            ),
        }
    }
}

/// Повторяет `attempt`, пока `busy` считает результат временно занятым замком.
///
/// Первый результат, который `busy` не признал временным, возвращается как есть.
/// По истечении `limit` возвращается [`LockWaitError::Timeout`] с последней
/// ошибкой и фактически выдержанным временем: бесконечного повтора нет.
pub(crate) fn await_release<T, E>(
    limit: Duration,
    mut attempt: impl FnMut() -> Result<T, E>,
    busy: impl Fn(&E) -> bool,
) -> Result<T, LockWaitError<E>> {
    let started = Instant::now();
    loop {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(error) if !busy(&error) => return Err(LockWaitError::Other(error)),
            Err(error) => {
                let waited = started.elapsed();
                if waited >= limit {
                    return Err(LockWaitError::Timeout {
                        last: error,
                        waited,
                    });
                }
                std::thread::sleep(RETRY_INTERVAL.min(limit - waited));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, OpenOptions, TryLockError};
    use std::path::{Path, PathBuf};

    /// Временный каталог с путём файла замка: удаляется вызывающим тестом.
    fn lock_in_temp_dir(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "anki-e2e-lock-wait-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lock");
        (dir, path)
    }

    fn open_lock(path: &Path) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap()
    }

    #[test]
    fn retry_stops_at_the_first_result_that_is_not_a_busy_lock() {
        let mut attempts = 0;
        let result = await_release(
            RELEASE_LIMIT,
            || {
                attempts += 1;
                if attempts == 1 {
                    Err::<(), u8>(1)
                } else {
                    Err::<(), u8>(2)
                }
            },
            |error| *error == 1,
        );
        assert!(matches!(result, Err(LockWaitError::Other(2))));
        assert_eq!(attempts, 2);
    }

    #[test]
    fn retry_returns_the_value_once_the_lock_is_free() {
        let mut attempts = 0;
        let result = await_release(
            RELEASE_LIMIT,
            || {
                attempts += 1;
                if attempts < 3 {
                    Err::<u8, u8>(1)
                } else {
                    Ok(attempts)
                }
            },
            |error| *error == 1,
        );
        assert!(matches!(result, Ok(3)), "{result:?}");
        assert_eq!(attempts, 3);
    }

    #[test]
    fn error_that_is_not_a_busy_lock_is_returned_without_waiting() {
        let started = Instant::now();
        let result: Result<(), LockWaitError<&str>> =
            await_release(RELEASE_LIMIT, || Err("нет прав"), |error| *error == "занят");
        assert!(matches!(result, Err(LockWaitError::Other("нет прав"))));
        assert!(started.elapsed() < RELEASE_LIMIT);
    }

    #[test]
    fn persistent_busy_lock_ends_with_a_diagnosable_error() {
        let limit = Duration::from_millis(30);
        let result: Result<(), LockWaitError<&str>> =
            await_release(limit, || Err("занят"), |error| *error == "занят");
        let error = result.unwrap_err();
        let text = error.to_string();
        assert!(text.contains("занят"), "{text}");
        assert!(text.contains("не освободился"), "{text}");
        let LockWaitError::Timeout { last, waited } = error else {
            panic!("ожидание не исчерпало срок: {text}");
        };
        assert_eq!(last, "занят");
        assert!(waited >= limit, "выдержано {waited:?}, срок {limit:?}");
    }

    /// Владелец освобождает настоящий `flock` только после отказа внутри ожидания.
    #[test]
    fn real_flock_released_after_a_busy_attempt_is_awaited() {
        let (dir, path) = lock_in_temp_dir("released");
        let owner = open_lock(&path);
        owner
            .try_lock()
            .unwrap_or_else(|error| panic!("владелец не взял замок: {error}"));
        let mut owner = Some(owner);
        let probe = open_lock(&path);
        let mut attempts = 0;
        let result = await_release(
            RELEASE_LIMIT,
            || {
                attempts += 1;
                let result = probe.try_lock();
                if attempts == 1 {
                    assert!(matches!(result, Err(TryLockError::WouldBlock)));
                    drop(owner.take());
                }
                result
            },
            |error| matches!(error, TryLockError::WouldBlock),
        );
        assert!(
            result.is_ok(),
            "замок не освобождён: {:?}",
            result.err().map(|error| error.to_string())
        );
        assert!(attempts >= 2, "попыток захвата: {attempts}");
        drop(probe);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Настоящая длительная блокировка не превращается в успех: ожидание
    /// исчерпывает срок и возвращает занятость, а не `Ok`.
    #[test]
    fn lock_held_beyond_the_limit_is_not_reported_as_released() {
        let (dir, path) = lock_in_temp_dir("held");
        let owner = open_lock(&path);
        owner
            .try_lock()
            .unwrap_or_else(|error| panic!("владелец не взял замок: {error}"));
        let probe = open_lock(&path);
        let result = await_release(
            Duration::from_millis(40),
            || probe.try_lock(),
            |error| matches!(error, TryLockError::WouldBlock),
        );
        assert!(
            matches!(result, Err(LockWaitError::Timeout { .. })),
            "{:?}",
            result.err().map(|error| error.to_string())
        );
        drop(probe);
        drop(owner);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
