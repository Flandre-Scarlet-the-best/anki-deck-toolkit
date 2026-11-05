//! Порты bridge для тестов.
//!
//! Порт из `bind(0)` лежит в диапазоне эфемерных портов ядра. После того как
//! тест его отпустил, ядро может выдать тот же номер исходящему соединению
//! параллельного теста или соединению эмулятора расширения с самим собой
//! (Linux соединяет сокет сам с собой, когда исходный порт совпал с целевым,
//! а на целевом никто не слушает). Тогда bridge получает `AddrInUse`, и код
//! честно отвечает `adapter_busy`/`bridge_port_busy` — тест падает случайно.
//!
//! Поэтому тестовый порт берётся вне эфемерного диапазона: исходящие
//! соединения его не получают. Между процессами тестов (`cargo nextest`,
//! одновременные прогоны) выбранный порт закрепляется файловой блокировкой
//! `<port>.lock` во временном каталоге до конца процесса; ОС снимает её и при
//! аварийном завершении. Пустые файлы блокировок не удаляются (удаление
//! открытого другим процессом файла ломает взаимное исключение); их не больше,
//! чем портов в интервале кандидатов.

use std::fs::{File, OpenOptions, TryLockError};
use std::net::{Ipv4Addr, TcpListener};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

const LOWEST: u32 = 20_000;
const HIGHEST: u32 = 65_535;
/// Меньше этого запаса вне эфемерного диапазона — последний вариант `bind(0)`.
const MIN_SPAN: u32 = 1_000;

/// Эфемерный диапазон: фактический на Linux, иначе диапазон IANA, которым
/// по умолчанию пользуются Windows и macOS.
fn ephemeral_range() -> (u32, u32) {
    let fallback = if cfg!(target_os = "linux") {
        (32_768, 60_999)
    } else {
        (49_152, 65_535)
    };
    std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
        .ok()
        .and_then(|range| {
            let mut bounds = range.split_whitespace().map(str::parse::<u32>);
            Some((bounds.next()?.ok()?, bounds.next()?.ok()?))
        })
        .unwrap_or(fallback)
}

/// Полуинтервал кандидатов `[start, end)` вне эфемерного диапазона.
fn candidates() -> Option<(u32, u32)> {
    let (low, high) = ephemeral_range();
    if low >= LOWEST + MIN_SPAN {
        Some((LOWEST, low))
    } else if HIGHEST.saturating_sub(high) >= MIN_SPAN {
        Some((high + 1, HIGHEST + 1))
    } else {
        None
    }
}

fn lock_dir() -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("anki-deck-toolkit-test-ports");
    std::fs::create_dir_all(&dir).ok().map(|()| dir)
}

fn lock_path(dir: &std::path::Path, port: u16) -> PathBuf {
    dir.join(format!("{port}.lock"))
}

/// Закрепляет порт за процессом. `None` — порт закреплён другим вызовом.
fn reserve(dir: Option<&std::path::Path>, port: u16) -> Option<Option<File>> {
    let Some(dir) = dir else {
        return Some(None);
    };
    let Ok(file) = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path(dir, port))
    else {
        // Каталог блокировок недоступен — остаётся проверка bind.
        return Some(None);
    };
    match file.try_lock() {
        Ok(()) => Some(Some(file)),
        Err(TryLockError::WouldBlock) => None,
        Err(TryLockError::Error(_)) => Some(None),
    }
}

fn os_assigned() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .and_then(|l| l.local_addr())
        .expect("тестовый порт")
        .port()
}

/// Свободный loopback-порт вне эфемерного диапазона ядра, закреплённый за
/// текущим процессом тестов.
pub(crate) fn free_port() -> u16 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    static HELD: Mutex<Vec<File>> = Mutex::new(Vec::new());
    let Some((first, end)) = candidates() else {
        return os_assigned();
    };
    let span = end - first;
    // Случайный старт каждого процесса: прогон подряд не берёт порты, на
    // которых ещё висят TIME_WAIT-соединения прошлого прогона и мешают
    // переподключению эмулятора. Совпадения разводит блокировка.
    static START: OnceLock<u32> = OnceLock::new();
    let start = *START.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        (nanos ^ std::process::id().wrapping_mul(7919)) % span
    });
    let dir = lock_dir();
    for _ in 0..span {
        let step = NEXT.fetch_add(1, Ordering::Relaxed);
        let port = u16::try_from(first + (start + step) % span).expect("порт в пределах u16");
        if port == super::DEFAULT_PORT {
            continue;
        }
        let Some(lock) = reserve(dir.as_deref(), port) else {
            continue;
        };
        if TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok() {
            HELD.lock().unwrap().extend(lock);
            return port;
        }
    }
    os_assigned()
}

#[test]
fn ports_are_distinct_reserved_and_outside_the_ephemeral_range() {
    let ports: Vec<u16> = (0..64).map(|_| free_port()).collect();
    let unique: std::collections::HashSet<_> = ports.iter().collect();
    assert_eq!(unique.len(), ports.len(), "{ports:?}");
    let (low, high) = ephemeral_range();
    if candidates().is_some() {
        assert!(
            ports.iter().all(|p| !(low..=high).contains(&u32::from(*p))),
            "{ports:?}"
        );
    }
    // Другой процесс увидит порт занятым: блокировка открытого файла
    // конфликтует и с повторным открытием в том же процессе.
    if let Some(dir) = lock_dir() {
        assert!(reserve(Some(&dir), ports[0]).is_none(), "{}", ports[0]);
    }
}
