//! Порты bridge для тестов.
//!
//! Порт из `bind(0)` лежит в диапазоне эфемерных портов ядра. После того как
//! тест его отпустил, ядро может выдать тот же номер исходящему соединению
//! параллельного теста или соединению эмулятора расширения с самим собой
//! (Linux соединяет сокет сам с собой, когда исходный порт совпал с целевым,
//! а на целевом никто не слушает). Тогда bridge получает `AddrInUse`, и код
//! честно отвечает `adapter_busy`/`bridge_port_busy` — тест падает случайно.
//!
//! Поэтому тестовый порт берётся ниже эфемерного диапазона: исходящие
//! соединения его не получают. Внутри процесса номера не повторяются, а
//! стартовое смещение зависит от PID, чтобы параллельные тестовые процессы
//! не перебирали одни и те же номера.

use std::net::{Ipv4Addr, TcpListener};
use std::sync::atomic::{AtomicU32, Ordering};

const LOWEST: u32 = 20_000;
/// Меньше этого запаса ниже эфемерного диапазона — откат к `bind(0)`.
const MIN_SPAN: u32 = 1_000;

fn ephemeral_low() -> Option<u32> {
    let range = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range").ok()?;
    range.split_whitespace().next()?.parse().ok()
}

fn os_assigned() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .and_then(|l| l.local_addr())
        .expect("тестовый порт")
        .port()
}

/// Свободный loopback-порт вне эфемерного диапазона ядра.
pub(crate) fn free_port() -> u16 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let Some(high) = ephemeral_low().filter(|low| *low >= LOWEST + MIN_SPAN) else {
        return os_assigned();
    };
    let span = high - LOWEST;
    let start = std::process::id().wrapping_mul(7919) % span;
    for _ in 0..span {
        let step = NEXT.fetch_add(1, Ordering::Relaxed);
        let port = LOWEST + (start + step) % span;
        let port = u16::try_from(port).expect("порт в пределах u16");
        if port != super::DEFAULT_PORT && TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok() {
            return port;
        }
    }
    os_assigned()
}

#[test]
fn ports_are_distinct_and_below_the_ephemeral_range() {
    let ports: Vec<u16> = (0..64).map(|_| free_port()).collect();
    let unique: std::collections::HashSet<_> = ports.iter().collect();
    assert_eq!(unique.len(), ports.len(), "{ports:?}");
    if let Some(low) = ephemeral_low().filter(|low| *low >= LOWEST + MIN_SPAN) {
        assert!(ports.iter().all(|p| u32::from(*p) < low), "{ports:?}");
    }
}
