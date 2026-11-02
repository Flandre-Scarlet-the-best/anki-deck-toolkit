//! Изолированный E2E-стенд Anki Desktop.
//!
//! На хосте бинарник управляет Docker и отчётом; внутри контейнера тот же
//! бинарник работает агентом (`agent`). Запуск — только явной командой: в
//! `cargo test`, CI и сборку стенд не встроен.

mod agent;
#[cfg(feature = "browser")]
mod browser;
mod cli;
mod docker;
mod host;
mod real_site;
mod release;
mod report;
mod source;
mod util;

use clap::Parser;
use cli::{Cli, Command};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Command::Agent(args) = cli.command {
        return agent::run(args);
    }
    if let Command::Resolve(args) = &cli.command {
        return ExitCode::from(host::resolve_only(args));
    }
    if let Command::Clean {
        images,
        cache,
        docker_config,
        work_dir,
    } = &cli.command
    {
        return ExitCode::from(host::clean(
            *images,
            *cache,
            *docker_config,
            work_dir.clone(),
        ));
    }
    let interrupted = Arc::new(AtomicBool::new(false));
    let flag = interrupted.clone();
    // Прерывание не убивает runner: он удаляет контейнер и пишет отчёт INTERRUPTED.
    if let Err(e) = ctrlc::set_handler(move || {
        eprintln!("\nПрерывание: останавливаю сценарии и удаляю контейнер…");
        flag.store(true, Ordering::SeqCst);
    }) {
        eprintln!("Обработчик прерывания не установлен: {e}");
    }
    #[cfg(feature = "browser")]
    if let Command::Browser(args) = &cli.command {
        return ExitCode::from(browser::run(args, interrupted));
    }
    if let Command::RealSite(args) = &cli.command {
        return ExitCode::from(real_site::run(args, interrupted));
    }
    let Some((suite, scenarios, args)) = cli.command.suite() else {
        return ExitCode::from(3);
    };
    ExitCode::from(host::run(suite, scenarios, args, interrupted))
}
