//! Управляемая синтетическая нагрузка для проверки исполнителя.
//!
//! Действия выполняются в порядке аргументов, поэтому одна команда задаёт
//! точную форму прогона: сколько памяти занять, сколько процессорного времени
//! потратить, какой код вернуть, оставить ли процесс-сироту. Программа не
//! читает пользовательских данных и пишет только по явно названным путям.

use rustix::time::{ClockId, clock_gettime};
use std::fs;
use std::io::{Read, Seek, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::time::Duration;

fn usage() -> ! {
    eprintln!(
        "benchmark-workload [--alloc-mib N] [--busy-ms N] [--threads N] [--sleep-ms N] [--exit N] \
         [--orphan-sleep-ms N] [--escape-sleep-ms N] [--ignore-sigint] [--touch FILE] [--append FILE TEXT] \
         [--fail-if-exists FILE] [--fail-on-call N --counter FILE] [--report-dir DIR --report JSON] \
         [--report-dir DIR --report-from FILE] [--write-bytes FILE N] [--print-json JSON] \
         [--file-io FILE MiB] [--require-offline] [--spawn-rest]"
    );
    std::process::exit(2)
}

fn number(value: Option<String>) -> u64 {
    value
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| usage())
}

/// Каждый поток тратит `duration` собственного процессорного времени, а не
/// времени по часам: на перегруженной машине работа длится дольше, но её
/// цена в CPU остаётся той, что заявлена.
fn busy(duration: Duration, threads: u64) {
    let workers: Vec<_> = (0..threads.max(1))
        .map(|_| {
            std::thread::spawn(move || {
                let cpu = || {
                    let t = clock_gettime(ClockId::ThreadCPUTime);
                    Duration::new(t.tv_sec as u64, t.tv_nsec as u32)
                };
                let start = cpu();
                let mut x = 0u64;
                while cpu() - start < duration {
                    for i in 0..10_000u64 {
                        x = x.wrapping_mul(6364136223846793005).wrapping_add(i);
                    }
                    std::hint::black_box(x);
                }
            })
        })
        .collect();
    for worker in workers {
        let _ = worker.join();
    }
}

fn write_report(root: Option<&PathBuf>, json: &str) {
    let root = root.unwrap_or_else(|| usage());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let dir = root.join(format!("{nanos}-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap_or_else(|_| std::process::exit(3));
    fs::write(dir.join("report.json"), json).unwrap_or_else(|_| std::process::exit(3));
}

/// Изолированный цикл: новая запись + fsync, сброс только её чистых страниц,
/// чтение и удаление. Советы ядро вправе игнорировать; io.stat это покажет.
fn file_io(path: PathBuf, mib: u64) -> std::io::Result<()> {
    if !(1..=256).contains(&mib) {
        usage();
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    let result = (|| {
        let chunk = [7u8; 65536];
        for _ in 0..mib * 16 {
            file.write_all(&chunk)?;
        }
        file.sync_all()?;
        rustix::fs::fadvise(&file, 0, None, rustix::fs::Advice::DontNeed)?;
        file.rewind()?;
        let mut chunk = [0u8; 65536];
        let mut total = 0;
        loop {
            let n = file.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            if chunk[..n].iter().any(|b| *b != 7) {
                return Err(std::io::Error::other("содержимое I/O-нагрузки изменилось"));
            }
            total += n as u64;
        }
        if total != mib * (1 << 20) {
            return Err(std::io::Error::other("I/O-нагрузка прочитана не полностью"));
        }
        Ok(())
    })();
    drop(file);
    let cleanup = fs::remove_file(path);
    result.and(cleanup)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut held: Vec<Vec<u8>> = Vec::new();
    let mut threads = 1;
    let mut counter: Option<PathBuf> = None;
    let mut report_dir: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--alloc-mib" => {
                let mib = number(args.next()) as usize;
                let mut block = vec![0u8; mib << 20];
                // Каждая страница записывается, чтобы стать резидентной.
                for page in block.chunks_mut(4096) {
                    page[0] = 1;
                }
                held.push(std::hint::black_box(block));
            }
            "--threads" => threads = number(args.next()),
            "--busy-ms" => busy(Duration::from_millis(number(args.next())), threads),
            "--sleep-ms" => std::thread::sleep(Duration::from_millis(number(args.next()))),
            "--exit" => std::process::exit(number(args.next()) as i32),
            "--orphan-sleep-ms" | "--escape-sleep-ms" => {
                let ms = number(args.next()).to_string();
                let mut child = std::process::Command::new(std::env::current_exe().unwrap());
                child.args(["--sleep-ms", &ms]);
                if arg == "--escape-sleep-ms" {
                    // Покинуть группу процессов, как демон.
                    child.process_group(0);
                }
                // Потомок намеренно не дожидается: он моделирует процесс,
                // переживший измеряемую цель.
                #[allow(clippy::zombie_processes)]
                child.spawn().unwrap_or_else(|_| usage());
            }
            "--ignore-sigint" => {
                let _ = ctrlc::set_handler(|| {});
            }
            "--touch" => {
                let path = PathBuf::from(args.next().unwrap_or_else(|| usage()));
                let mut file = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .unwrap_or_else(|_| std::process::exit(3));
                let _ = writeln!(file, "x");
            }
            "--append" => {
                let path = PathBuf::from(args.next().unwrap_or_else(|| usage()));
                let text = args.next().unwrap_or_else(|| usage());
                let mut file = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .unwrap_or_else(|_| std::process::exit(3));
                let _ = writeln!(file, "{text}");
            }
            "--fail-if-exists" => {
                let path = PathBuf::from(args.next().unwrap_or_else(|| usage()));
                if path.exists() {
                    std::process::exit(3);
                }
            }
            "--counter" => counter = Some(PathBuf::from(args.next().unwrap_or_else(|| usage()))),
            "--fail-on-call" => {
                let failing = number(args.next());
                let path = counter.clone().unwrap_or_else(|| usage());
                let calls = fs::read_to_string(&path)
                    .ok()
                    .and_then(|t| t.trim().parse::<u64>().ok())
                    .unwrap_or(0)
                    + 1;
                let _ = fs::write(&path, calls.to_string());
                if calls == failing {
                    std::process::exit(1);
                }
            }
            "--report-dir" => {
                report_dir = Some(PathBuf::from(args.next().unwrap_or_else(|| usage())))
            }
            "--report" => {
                let json = args.next().unwrap_or_else(|| usage());
                write_report(report_dir.as_ref(), &json);
            }
            // Текст отчёта читается из файла в момент запуска; пустой файл —
            // запуск без отчёта.
            "--report-from" => {
                let path = PathBuf::from(args.next().unwrap_or_else(|| usage()));
                let json = fs::read_to_string(path).unwrap_or_else(|_| std::process::exit(3));
                if !json.is_empty() {
                    write_report(report_dir.as_ref(), &json);
                }
            }
            // Остальные действия выполняет потомок, которого процесс не
            // дожидается: он моделирует шаг, чья работа пережила его выход.
            "--spawn-rest" => {
                let rest: Vec<String> = args.by_ref().collect();
                let mut child = std::process::Command::new(std::env::current_exe().unwrap());
                child.args(rest);
                #[allow(clippy::zombie_processes)]
                child.spawn().unwrap_or_else(|_| usage());
            }
            "--file-io" => {
                let path = PathBuf::from(args.next().unwrap_or_else(|| usage()));
                let mib = number(args.next());
                file_io(path, mib).unwrap_or_else(|error| {
                    eprintln!("I/O-нагрузка: {error}");
                    std::process::exit(3)
                });
            }
            "--write-bytes" => {
                let path = PathBuf::from(args.next().unwrap_or_else(|| usage()));
                let bytes = number(args.next()) as usize;
                if let Some(parent) = path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                fs::write(path, vec![7u8; bytes]).unwrap_or_else(|_| std::process::exit(3));
            }
            "--print-json" => println!("{}", args.next().unwrap_or_else(|| usage())),
            "--require-offline" => {
                let interfaces: Vec<String> = fs::read_to_string("/proc/self/net/dev")
                    .unwrap_or_default()
                    .lines()
                    .skip(2)
                    .filter_map(|l| l.split_once(':').map(|(n, _)| n.trim().to_owned()))
                    .collect();
                if interfaces != ["lo"] {
                    std::process::exit(4);
                }
            }
            _ => usage(),
        }
    }
    drop(held);
}
