//! Хостовый контроллер: предпосылки, выбор релиза, образы, один контейнер,
//! исполнение агента, сбор свидетельств, отчёт и уборка.

use crate::cli::{DockerConfig, Keep, ReleaseArgs, RunArgs};
use crate::docker::{
    self, Docker, Exec, ExecEnd, LABEL, LABEL_KEEP, LABEL_KIND, LABEL_PID, LABEL_RUN, LABEL_RUNNER,
};
use crate::release::{self, Arch, GitHub, HttpsDownloader, Request, Resolved};
use crate::report::{self, Status, Stop};
use crate::source;
use crate::util::{self, sha256_hex};
use serde_json::{Value, json};
use std::fs;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Базовый образ закреплён по digest: обновление — осознанная правка.
pub const BASE_IMAGE: &str =
    "ubuntu:24.04@sha256:68ee22004a9109b44973313b02ff7b1de250f1de0f5ecb628d73f579aaff33ac";
const ANKI_DOCKERFILE: &str = include_str!("../docker/anki.Dockerfile");
const RUN_DOCKERFILE: &str = include_str!("../docker/run.Dockerfile");

struct Log {
    file: Mutex<Option<fs::File>>,
}

/// Сериализует доступ к Docker-образам и общим кэшам между параллельными
/// запусками. Освобождается после старта контейнера: Docker уже удерживает его
/// образ, а снимок исходников больше не нужен.
struct RunnerLock {
    _file: fs::File,
}

impl RunnerLock {
    fn lock_path() -> PathBuf {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
            .unwrap_or_else(|| {
                let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
                std::env::temp_dir().join(format!("anki-e2e-{user}"))
            });
        base.join("anki-e2e-runner.lock")
    }

    fn acquire() -> io::Result<Self> {
        let path = Self::lock_path();
        Self::acquire_at(&path, &|| false)
    }

    fn acquire_interruptible(interrupted: &AtomicBool) -> io::Result<Self> {
        let path = Self::lock_path();
        Self::acquire_at(&path, &|| interrupted.load(Ordering::SeqCst))
    }

    fn acquire_at(path: &Path, interrupted: &dyn Fn() -> bool) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        loop {
            if interrupted() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "прервано пользователем",
                ));
            }
            match file.try_lock() {
                Ok(()) if interrupted() => {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "прервано пользователем",
                    ));
                }
                Ok(()) => return Ok(Self { _file: file }),
                Err(fs::TryLockError::WouldBlock) => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(fs::TryLockError::Error(error)) => return Err(error),
            }
        }
    }
}

impl Log {
    fn line(&self, text: &str) {
        println!("{text}");
        if let Ok(mut guard) = self.file.lock()
            && let Some(f) = guard.as_mut()
        {
            let _ = writeln!(f, "{} {text}", util::utc_rfc3339(util::unix_now()));
        }
    }
}

fn work_dir(repo: &Path, args: &ReleaseArgs) -> PathBuf {
    args.work_dir
        .clone()
        .unwrap_or_else(|| repo.join("target").join("anki-e2e"))
}

fn request(args: &ReleaseArgs) -> Result<Request, Stop> {
    if args.offline {
        let sha = args.sha256.clone().unwrap_or_default().to_ascii_lowercase();
        if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Stop::error(
                "arguments",
                "invalid_sha256",
                "--sha256 должен содержать 64 hex-символа",
            ));
        }
        return Ok(Request::Offline {
            version: args.anki_version.clone().unwrap_or_default(),
            sha256: sha,
            archive: args.anki_archive.clone(),
        });
    }
    Ok(match &args.anki_version {
        Some(v) => Request::Explicit {
            version: v.clone(),
            allow_prerelease: args.allow_prerelease,
        },
        None => Request::Latest,
    })
}

fn resolve_and_acquire(
    args: &ReleaseArgs,
    arch: Arch,
    cache: &Path,
    interrupted: Option<&AtomicBool>,
) -> Result<(Resolved, release::Artifact), Stop> {
    let request = request(args)?;
    let resolved = match &request {
        Request::Offline { .. } => release::resolve(&NoNetwork, &request, arch)?,
        _ => {
            let github =
                GitHub::new().map_err(|e| Stop::error("resolve_anki_release", "http_client", e))?;
            release::resolve(&github, &request, arch)?
        }
    };
    let offline_archive = match &request {
        Request::Offline { archive, .. } => archive.clone(),
        _ => None,
    };
    let downloader = HttpsDownloader;
    let downloader = (!args.offline).then_some(&downloader as &dyn release::Downloader);
    let artifact = if let Some(interrupted) = interrupted {
        release::acquire_interruptible(
            cache,
            &resolved,
            offline_archive.as_deref(),
            downloader,
            args.allow_unverified_archive,
            interrupted,
        )?
    } else {
        release::acquire(
            cache,
            &resolved,
            offline_archive.as_deref(),
            downloader,
            args.allow_unverified_archive,
        )?
    };
    Ok((resolved, artifact))
}

/// Офлайн-режим не имеет доступа к индексу релизов.
struct NoNetwork;
impl release::ReleaseSource for NoNetwork {
    fn page(&self, _: u32) -> Result<Vec<release::ReleaseJson>, String> {
        Err("офлайн-режим".into())
    }
    fn by_tag(&self, _: &str) -> Result<Option<release::ReleaseJson>, String> {
        Err("офлайн-режим".into())
    }
}

pub fn resolve_only(args: &ReleaseArgs) -> u8 {
    let cwd = std::env::current_dir().unwrap_or_default();
    let repo = source::repository_root(&cwd).unwrap_or(cwd);
    let work = work_dir(&repo, args);
    let _lock = match RunnerLock::acquire() {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("Не удалось заблокировать ресурсы anki-e2e: {e}");
            return Status::Error.exit_code();
        }
    };
    let docker = Docker::new(None);
    let arch = docker
        .text(&["info", "--format", "{{.Architecture}}"])
        .ok()
        .and_then(|a| Arch::from_docker(&a))
        .or_else(|| Arch::from_docker(std::env::consts::ARCH));
    let Some(arch) = arch else {
        eprintln!("Неподдерживаемая архитектура");
        return Status::Blocked.exit_code();
    };
    match resolve_and_acquire(args, arch, &work.join("cache").join("anki"), None) {
        Ok((resolved, artifact)) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({"anki":resolved,"artifact":artifact}))
                    .unwrap_or_default()
            );
            0
        }
        Err(stop) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({"stop":stop})).unwrap_or_default()
            );
            stop.status.exit_code()
        }
    }
}

/// `clean` удаляет контейнеры стенда, кроме контейнеров живого runner и
/// другого хоста; `--images` — образы с меткой стенда; `--cache` — локальные
/// `cache`/`build` рабочего каталога и BuildKit cache mounts Cargo стенда.
/// Обычный слоевой кэш BuildKit общий для демона и меток стенда не несёт: он не
/// трогается. Любой неудалённый требуемый ресурс — код ошибки.
pub fn clean(images: bool, cache: bool, docker_config: DockerConfig, work: Option<PathBuf>) -> u8 {
    let cwd = std::env::current_dir().unwrap_or_default();
    let repo = source::repository_root(&cwd).unwrap_or(cwd);
    let work = work.unwrap_or_else(|| repo.join("target").join("anki-e2e"));
    let _lock = match RunnerLock::acquire() {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("Не удалось заблокировать ресурсы anki-e2e: {e}");
            return Status::Error.exit_code();
        }
    };
    let config_dir = match docker_config {
        DockerConfig::Inherit => None,
        DockerConfig::Isolated => {
            if let Some(context) = docker::user_context_requires_inherit() {
                eprintln!(
                    "Активен контекст Docker «{context}»; используйте --docker-config inherit или DOCKER_HOST"
                );
                return Status::Blocked.exit_code();
            }
            match isolated_config(&work.join("docker-config")) {
                Ok(path) => Some(path),
                Err(e) => {
                    eprintln!("Не удалось создать изолированную конфигурацию Docker: {e}");
                    return Status::Error.exit_code();
                }
            }
        }
    };
    let docker = Docker::new(config_dir);
    let local = HostIdentity::current();
    let mut failures = Vec::new();
    match docker.owned_containers() {
        Ok(list) => {
            for c in list {
                let id = c["ID"].as_str().unwrap_or("");
                let name = c["Names"].as_str().unwrap_or("");
                let labels = c["Labels"].as_str().unwrap_or("");
                match local_runner_state(labels, local.as_ref()) {
                    RunnerState::Alive => {
                        println!("Пропущен контейнер активного прогона {name} ({id})");
                        continue;
                    }
                    RunnerState::Foreign => {
                        println!("Пропущен контейнер прогона с другого хоста {name} ({id})");
                        continue;
                    }
                    RunnerState::Stale => {}
                }
                match docker.remove_container(id) {
                    Ok(()) => println!("Удалён контейнер {name} ({id})"),
                    Err(e) => failures.push(format!("контейнер {name} ({id}): {e}")),
                }
            }
        }
        Err(e) => {
            failures.push(format!("список контейнеров стенда: {e}"));
        }
    }
    if images {
        match docker.owned_images() {
            Ok(ids) => {
                for id in ids {
                    match docker.text(&["image", "rm", "-f", &id]) {
                        Ok(_) => println!("Удалён образ {id}"),
                        Err(e) => failures.push(format!("образ {id}: {e}")),
                    }
                }
            }
            Err(e) => failures.push(format!("список образов стенда: {e}")),
        }
    }
    if cache {
        for sub in ["cache", "build"] {
            let p = work.join(sub);
            if !p.exists() {
                continue;
            }
            match fs::remove_dir_all(&p) {
                Ok(()) => println!("Удалён {}", p.display()),
                Err(e) => failures.push(format!("{}: {e}", p.display())),
            }
        }
        match docker.prune_cache_mounts() {
            Ok(()) => match docker.remaining_cache_mounts() {
                Ok(left) if left.is_empty() => println!(
                    "Удалены BuildKit cache mounts стенда: {}",
                    docker::CACHE_MOUNTS.join(", ")
                ),
                Ok(left) => failures.push(format!(
                    "BuildKit cache mounts стенда не удалены: {}",
                    left.join(", ")
                )),
                Err(e) => failures.push(format!("проверка BuildKit cache mounts: {e}")),
            },
            Err(e) => failures.push(format!("BuildKit cache mounts: {e}")),
        }
        println!("Слоевой кэш BuildKit демона общий и не удаляется (см. README)");
    }
    if failures.is_empty() {
        return 0;
    }
    for failure in &failures {
        eprintln!("Не удалено: {failure}");
    }
    Status::Error.exit_code()
}

fn isolated_config(dir: &Path) -> std::io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let file = dir.join("config.json");
    if !file.exists() {
        fs::write(&file, b"{}\n")?;
    }
    Ok(dir.to_path_buf())
}

fn hard_link_or_copy(from: &Path, to: &Path) -> std::io::Result<()> {
    if to.exists() {
        fs::remove_file(to)?;
    }
    fs::hard_link(from, to).or_else(|_| fs::copy(from, to).map(|_| ()))
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

struct Run {
    id: String,
    dir: PathBuf,
    log: Arc<Log>,
    report: Value,
    started: f64,
}

impl Run {
    /// Первая остановка — первичная причина; поздние (уборка, сбор свидетельств)
    /// не затирают её и сохраняются в `secondary_stops`.
    fn stop(&mut self, stop: Stop) -> Status {
        self.log.line(&format!(
            "{} [{}] {}: {}",
            stop.status.as_str(),
            stop.stage,
            stop.reason,
            stop.message
        ));
        let status = stop.status;
        self.record_stop(json!(stop));
        status
    }

    fn record_stop(&mut self, stop: Value) {
        if self.report["stop"].is_null() {
            self.report["stop"] = stop;
        } else if let Some(list) = self.report["secondary_stops"].as_array_mut() {
            list.push(stop);
        } else {
            self.report["secondary_stops"] = json!([stop]);
        }
    }
}

pub fn run(
    suite: &str,
    scenarios: Vec<String>,
    args: &RunArgs,
    interrupted: Arc<AtomicBool>,
) -> u8 {
    let cwd = std::env::current_dir().unwrap_or_default();
    let repo = match source::repository_root(&cwd) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Запускайте из рабочей копии anki-deck-toolkit: {e}");
            return Status::Error.exit_code();
        }
    };
    for name in &scenarios {
        if !crate::cli::FULL.contains(&name.as_str()) {
            eprintln!(
                "Неизвестный сценарий {name}; доступны: {}",
                crate::cli::FULL.join(", ")
            );
            return Status::Error.exit_code();
        }
    }
    let work = work_dir(&repo, &args.release);
    let started = util::unix_now();
    let id = format!(
        "{}-{}",
        util::utc_compact(started),
        util::random_hex(3).unwrap_or_default()
    );
    let dir = work.join("runs").join(&id);
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("{}: {e}", dir.display());
        return Status::Error.exit_code();
    }
    let log = Arc::new(Log {
        file: Mutex::new(fs::File::create(dir.join("runner.log")).ok()),
    });
    let mut run = Run {
        id: id.clone(),
        dir: dir.clone(),
        log: log.clone(),
        started,
        report: json!({
            "schema_version": 1, "run_id": id, "suite": suite, "scenarios_requested": scenarios,
            "started_at": util::utc_rfc3339(started), "command": std::env::args().collect::<Vec<_>>(),
            "host": {"os":std::env::consts::OS,"arch":std::env::consts::ARCH},
            "paths": {"run_dir": dir, "work_dir": work},
        }),
    };
    log.line(&format!(
        "anki-e2e {suite}: прогон {id}, отчёт {}",
        dir.display()
    ));
    let status = execute(&mut run, &repo, &work, &scenarios, args, &interrupted);
    finish(&mut run, status)
}

fn finish(run: &mut Run, status: Status) -> u8 {
    let finished = util::unix_now();
    run.report["status"] = json!(status);
    run.report["finished_at"] = json!(util::utc_rfc3339(finished));
    run.report["duration_s"] = json!(finished - run.started);
    let md_path = run.dir.join("report.md");
    let errors = write_reports(run);
    if errors.is_empty() {
        run.log.line(&format!(
            "Итог: {} — {}",
            status.as_str(),
            md_path.display()
        ));
        return status.exit_code();
    }
    // Без отчёта прогон не может быть успешным; неуспешный итог и его первичная
    // причина сохраняются, а ошибка записи добавляется к ним.
    let final_status = if status == Status::Pass {
        Status::Error
    } else {
        status
    };
    run.report["status"] = json!(final_status);
    run.report["report_write_errors"] = json!(errors);
    // Повторная попытка: доступный файл должен отражать неполноту отчёта.
    let _ = write_reports(run);
    let primary = run.report["stop"]["reason"]
        .as_str()
        .map(|r| format!(", первичная причина {r}"))
        .unwrap_or_default();
    run.log.line(&format!(
        "Итог: {} (результат сценариев {}{primary}); отчёт не сохранён: {}",
        final_status.as_str(),
        status.as_str(),
        errors.join("; ")
    ));
    final_status.exit_code()
}

fn write_reports(run: &Run) -> Vec<String> {
    let mut errors = Vec::new();
    if let Err(e) = util::write_json(&run.dir.join("report.json"), &run.report) {
        errors.push(e);
    }
    if let Err(e) = util::write_atomic(
        &run.dir.join("report.md"),
        report::render_markdown(&run.report).as_bytes(),
    ) {
        errors.push(e);
    }
    errors
}

fn execute(
    run: &mut Run,
    repo: &Path,
    work: &Path,
    scenarios: &[String],
    args: &RunArgs,
    interrupted: &AtomicBool,
) -> Status {
    let log = run.log.clone();
    let config_dir = match args.docker_config {
        DockerConfig::Isolated => {
            if let Some(context) = docker::user_context_requires_inherit() {
                return run.stop(Stop::blocked(
                    "docker_preflight",
                    "docker_context_requires_inherit",
                    format!("Активен контекст Docker «{context}»; используйте --docker-config inherit или DOCKER_HOST"),
                ));
            }
            match isolated_config(&work.join("docker-config")) {
                Ok(p) => Some(p),
                Err(e) => {
                    return run.stop(Stop::error(
                        "docker_preflight",
                        "docker_config",
                        e.to_string(),
                    ));
                }
            }
        }
        DockerConfig::Inherit => None,
    };
    let docker = Docker::new(config_dir);
    log.line("Проверка Docker…");
    let info = match docker.preflight() {
        Ok(i) => i,
        Err(stop) => return run.stop(stop),
    };
    run.report["docker"] = info.clone();
    let Some(arch) = info["architecture"].as_str().and_then(Arch::from_docker) else {
        return run.stop(Stop::blocked(
            "docker_preflight",
            "unsupported_architecture",
            format!(
                "Архитектура Docker {} не поддерживается официальной Linux Anki",
                info["architecture"]
            ),
        ));
    };
    log.line("Ожидание блокировки общих ресурсов anki-e2e…");
    let startup_lock = match RunnerLock::acquire_interruptible(interrupted) {
        Ok(lock) => lock,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => {
            return run.stop(Stop {
                status: Status::Interrupted,
                stage: "runner_lock",
                reason: "interrupted".into(),
                message: "Прервано пользователем во время ожидания блокировки общих ресурсов"
                    .into(),
            });
        }
        Err(e) => {
            return run.stop(Stop::error(
                "docker_preflight",
                "runner_lock",
                e.to_string(),
            ));
        }
    };
    run.report["cleanup"]["stale"] = json!(remove_stale(&docker, &log));

    log.line("Выбор релиза Anki по первичному источнику…");
    let (resolved, artifact) = match resolve_and_acquire(
        &args.release,
        arch,
        &work.join("cache").join("anki"),
        Some(interrupted),
    ) {
        Ok(v) => v,
        Err(stop) => return run.stop(stop),
    };
    log.line(&format!(
        "Anki {} ({}; последняя стабильная: {}), архив {} [{}]",
        resolved.version,
        resolved.mode,
        resolved
            .latest_stable
            .as_deref()
            .unwrap_or("не проверялась"),
        short(&artifact.sha256),
        artifact.verification
    ));
    run.report["anki"] = json!(resolved);
    run.report["artifact"] = json!(artifact);
    if interrupted.load(Ordering::SeqCst) {
        return run.stop(Stop {
            status: Status::Interrupted,
            stage: "resolve_anki_release",
            reason: "interrupted".into(),
            message: "Прервано пользователем".into(),
        });
    }

    log.line("Снимок исходников toolkit…");
    let build = work.join("build");
    let staging = build.join(format!("src-staging-{}", run.id));
    // Рабочий каталог стенда (кэши, снимки, отчёты) в снимок не входит.
    let provenance = match source::snapshot(repo, &staging.join("src"), work) {
        Ok(p) => p,
        Err(e) => return run.stop(Stop::error("source_snapshot", "snapshot_failed", e)),
    };
    run.report["source"] = json!(provenance);
    if args.require_clean && provenance.dirty {
        let _ = fs::remove_dir_all(&staging);
        return run.stop(Stop::blocked(
            "source_snapshot",
            "dirty_worktree",
            format!("Незакоммиченные изменения: {:?}", provenance.dirty_paths),
        ));
    }
    let context = build.join(format!("src-{}", short(&provenance.tree_sha256)));
    if context.exists() {
        let _ = fs::remove_dir_all(&staging);
    } else if let Err(e) = fs::rename(&staging, &context) {
        return run.stop(Stop::error("source_snapshot", "snapshot_io", e.to_string()));
    }

    // Образ Anki: ключ — версия, архитектура, SHA-256 архива, Dockerfile и базовый образ.
    let anki_key = sha256_hex(format!("{ANKI_DOCKERFILE}\n{BASE_IMAGE}").as_bytes());
    let anki_tag = format!(
        "anki-e2e/anki:{}-{}-{}-{}",
        resolved.version,
        arch.as_str(),
        short(&artifact.sha256),
        short(&anki_key)
    );
    let anki_ctx = build.join(format!("anki-{}", short(&artifact.sha256)));
    let labels = |kind: &str| {
        vec![
            (LABEL.to_owned(), "1".to_owned()),
            (LABEL_KIND.to_owned(), kind.to_owned()),
        ]
    };
    if args.rebuild || docker.image_id(&anki_tag).is_none() {
        log.line(&format!("Сборка образа {anki_tag}…"));
        let prepared = fs::create_dir_all(&anki_ctx)
            .and_then(|()| hard_link_or_copy(&artifact.path, &anki_ctx.join("anki.tar.zst")))
            .and_then(|()| fs::write(anki_ctx.join("Dockerfile"), ANKI_DOCKERFILE));
        if let Err(e) = prepared {
            return run.stop(Stop::error("image_build", "build_context", e.to_string()));
        }
        let build_args = vec![
            ("BASE_IMAGE".to_owned(), BASE_IMAGE.to_owned()),
            ("ANKI_VERSION".to_owned(), resolved.version.clone()),
            ("ANKI_SHA256".to_owned(), artifact.sha256.clone()),
            ("ANKI_ARCHIVE".to_owned(), "anki.tar.zst".to_owned()),
        ];
        if let Err(e) = docker.build(&docker::Build {
            dockerfile: &anki_ctx.join("Dockerfile"),
            context: &anki_ctx,
            tag: &anki_tag,
            args: &build_args,
            labels: &labels("anki"),
            platform: arch.docker_platform(),
            log: &run.dir.join("build-anki.log"),
            no_cache: args.rebuild,
            interrupted: &|| interrupted.load(Ordering::SeqCst),
        }) {
            if interrupted.load(Ordering::SeqCst) {
                return run.stop(Stop {
                    status: Status::Interrupted,
                    stage: "image_build",
                    reason: "interrupted".into(),
                    message: e,
                });
            }
            return run.stop(Stop::error("image_build", "anki_image_build_failed", e));
        }
    } else {
        log.line(&format!(
            "Образ {anki_tag} уже собран из того же проверенного архива"
        ));
    }
    let anki_id = docker.image_id(&anki_tag);
    let rust_image = format!("rust:{}-bookworm", provenance.msrv);
    let run_tag = format!(
        "anki-e2e/run:{}-{}",
        short(&provenance.tree_sha256),
        short(&sha256_hex(
            format!("{anki_tag}{RUN_DOCKERFILE}{rust_image}").as_bytes()
        ))
    );
    if args.rebuild || docker.image_id(&run_tag).is_none() {
        log.line(&format!("Сборка anki-repo и агента из снимка в {run_tag}…"));
        if let Err(e) = fs::write(context.join("Dockerfile"), RUN_DOCKERFILE) {
            return run.stop(Stop::error("image_build", "build_context", e.to_string()));
        }
        let build_args = vec![
            ("ANKI_IMAGE".to_owned(), anki_tag.clone()),
            ("RUST_IMAGE".to_owned(), rust_image.clone()),
            ("RUST_VERSION".to_owned(), provenance.msrv.clone()),
            ("SOURCE_COMMIT".to_owned(), provenance.commit.clone()),
            (
                "SOURCE_TREE_SHA256".to_owned(),
                provenance.tree_sha256.clone(),
            ),
        ];
        if let Err(e) = docker.build(&docker::Build {
            dockerfile: &context.join("Dockerfile"),
            context: &context,
            tag: &run_tag,
            args: &build_args,
            labels: &labels("run"),
            platform: arch.docker_platform(),
            log: &run.dir.join("build-run.log"),
            no_cache: args.rebuild,
            interrupted: &|| interrupted.load(Ordering::SeqCst),
        }) {
            if interrupted.load(Ordering::SeqCst) {
                return run.stop(Stop {
                    status: Status::Interrupted,
                    stage: "image_build",
                    reason: "interrupted".into(),
                    message: e,
                });
            }
            return run.stop(Stop::error("image_build", "run_image_build_failed", e));
        }
    } else {
        log.line(&format!(
            "Образ {run_tag} уже собран из того же снимка исходников"
        ));
    }
    run.report["images"] = json!({
        "anki": {"tag":anki_tag,"id":anki_id,"base_image":BASE_IMAGE,"dockerfile_sha256":sha256_hex(ANKI_DOCKERFILE.as_bytes())},
        "run": {"tag":run_tag,"id":docker.image_id(&run_tag),"builder_image":rust_image,"dockerfile_sha256":sha256_hex(RUN_DOCKERFILE.as_bytes())},
    });
    run.report["pruned"] = prune_superseded(&docker, &run_tag, &build, &context, &log);

    let status = run_container(
        run,
        &docker,
        &run_tag,
        &resolved,
        scenarios,
        args,
        arch,
        startup_lock,
        interrupted,
    );
    if interrupted.load(Ordering::SeqCst) && status != Status::Error {
        return Status::Interrupted;
    }
    status
}

/// Образы запуска и снимки исходников прежних ревизий: каждая правка рабочей
/// копии даёт новые, и без уборки они копятся. Образы Anki и кэш архивов не
/// трогаются; образ, занятый сохранённым контейнером, Docker удалить не даст.
fn prune_superseded(
    docker: &Docker,
    keep_tag: &str,
    build: &Path,
    keep_context: &Path,
    log: &Log,
) -> Value {
    let mut images = Vec::new();
    for tag in docker.owned_image_tags("run").unwrap_or_default() {
        if tag != keep_tag
            && docker
                .output(&["image", "rm", &tag])
                .is_ok_and(|o| o.status.success())
        {
            images.push(tag);
        }
    }
    let mut contexts = Vec::new();
    for entry in fs::read_dir(build).into_iter().flatten().flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("src-") && path != keep_context && fs::remove_dir_all(&path).is_ok() {
            contexts.push(name);
        }
    }
    if !images.is_empty() || !contexts.is_empty() {
        log.line(&format!(
            "Удалены устаревшие образы запуска: {}, снимки исходников: {}",
            images.len(),
            contexts.len()
        ));
    }
    json!({"run_images": images, "source_snapshots": contexts})
}

/// Контейнеры прошлых прогонов этого хоста, чей runner уже не работает (без пометки keep).
fn remove_stale(docker: &Docker, log: &Log) -> Value {
    let mut removed = Vec::new();
    let local = HostIdentity::current();
    for c in docker.owned_containers().unwrap_or_default() {
        let labels = c["Labels"].as_str().unwrap_or("");
        if label_value(labels, LABEL_KEEP).as_deref() == Some("1") {
            continue;
        }
        if local_runner_state(labels, local.as_ref()) == RunnerState::Stale {
            let id = c["ID"].as_str().unwrap_or("").to_owned();
            if docker.remove_container(&id).is_ok() {
                log.line(&format!(
                    "Удалён оставшийся контейнер прошлого прогона {id}"
                ));
                removed.push(json!({
                    "id":id,
                    "name":c["Names"],
                    "run":label_value(labels, LABEL_RUN)
                }));
            }
        }
    }
    json!(removed)
}

fn label_value(labels: &str, name: &str) -> Option<String> {
    labels
        .split(',')
        .find_map(|kv| kv.strip_prefix(&format!("{name}=")))
        .map(str::to_owned)
}

/// Идентичность хоста runner: машина и загрузка ядра.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostIdentity {
    machine: String,
    boot: String,
}

impl HostIdentity {
    fn current() -> Option<HostIdentity> {
        let read = |path: &str| {
            fs::read_to_string(path)
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty() && !s.contains([':', ',']))
        };
        Some(HostIdentity {
            machine: read("/etc/machine-id").or_else(|| read("/var/lib/dbus/machine-id"))?,
            boot: read("/proc/sys/kernel/random/boot_id")?,
        })
    }
}

/// Время старта процесса в тактах с загрузки (поле 22 `/proc/<pid>/stat`).
fn process_start_ticks(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(19)?.parse().ok()
}

fn process_name(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_owned())
}

/// Значение метки [`LABEL_RUNNER`] для текущего процесса.
fn runner_label() -> Option<String> {
    let host = HostIdentity::current()?;
    let start = process_start_ticks(std::process::id())?;
    Some(format!("{}:{}:{start}", host.machine, host.boot))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunnerState {
    Alive,
    Stale,
    /// Контейнер создан runner другого хоста: его жизнь отсюда не проверить.
    Foreign,
}

fn local_runner_state(labels: &str, local: Option<&HostIdentity>) -> RunnerState {
    runner_state(labels, local, &process_start_ticks, &process_name)
}

/// Живым считается только тот процесс, который записал метку: тот же хост,
/// та же загрузка и то же время старта. Повторно выданный PID — не доказательство.
/// Контейнеры без метки идентичности (прежняя схема) считаются живыми, только
/// если по PID работает именно `anki-e2e`.
fn runner_state(
    labels: &str,
    local: Option<&HostIdentity>,
    start_of: &dyn Fn(u32) -> Option<u64>,
    name_of: &dyn Fn(u32) -> Option<String>,
) -> RunnerState {
    let Some(pid) = label_value(labels, LABEL_PID).and_then(|pid| pid.parse::<u32>().ok()) else {
        return RunnerState::Stale;
    };
    let Some(identity) = label_value(labels, LABEL_RUNNER) else {
        return match start_of(pid) {
            Some(_) if name_of(pid).as_deref() == Some("anki-e2e") => RunnerState::Alive,
            _ => RunnerState::Stale,
        };
    };
    let mut parts = identity.splitn(3, ':');
    let (Some(machine), Some(boot), Some(start)) = (parts.next(), parts.next(), parts.next())
    else {
        return RunnerState::Alive;
    };
    let Some(local) = local else {
        // Без собственной идентичности нельзя доказать, что runner завершился.
        return RunnerState::Alive;
    };
    if machine != local.machine {
        return RunnerState::Foreign;
    }
    if boot != local.boot {
        return RunnerState::Stale;
    }
    match (start.parse::<u64>().ok(), start_of(pid)) {
        (Some(expected), Some(actual)) if expected == actual => RunnerState::Alive,
        _ => RunnerState::Stale,
    }
}

#[allow(clippy::too_many_arguments)]
fn run_container(
    run: &mut Run,
    docker: &Docker,
    image: &str,
    resolved: &Resolved,
    scenarios: &[String],
    args: &RunArgs,
    arch: Arch,
    startup_lock: RunnerLock,
    interrupted: &AtomicBool,
) -> Status {
    let log = run.log.clone();
    let name = format!("anki-e2e-{}", run.id.to_lowercase());
    let keep_label = if args.keep_container == Keep::Never {
        "0"
    } else {
        "1"
    };
    let mut create: Vec<String> = [
        "create",
        "--name",
        &name,
        "--platform",
        arch.docker_platform(),
        "--init",
        "--user",
        "10001:10001",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--network",
        "bridge",
        "--cpus",
        &args.cpus,
        "--memory",
        &args.memory,
        "--memory-swap",
        &args.memory,
        "--shm-size",
        "1g",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    create.extend(["--pids-limit".into(), args.pids_limit.to_string()]);
    for (k, v) in [
        (LABEL, "1".to_owned()),
        (LABEL_KIND, "container".to_owned()),
        (LABEL_RUN, run.id.clone()),
        (LABEL_PID, std::process::id().to_string()),
        (LABEL_KEEP, keep_label.to_owned()),
    ] {
        create.extend(["--label".into(), format!("{k}={v}")]);
    }
    if let Some(identity) = runner_label() {
        create.extend(["--label".into(), format!("{LABEL_RUNNER}={identity}")]);
    }
    if args.vnc {
        create.extend(["-p".into(), format!("127.0.0.1:{}:5900", args.vnc_port)]);
    }
    create.extend([image.to_owned(), "sleep".into(), "infinity".into()]);
    let refs: Vec<&str> = create.iter().map(String::as_str).collect();
    let container = match docker.text(&refs) {
        Ok(id) => id,
        Err(e) => return run.stop(Stop::error("container", "container_create_failed", e)),
    };
    run.report["container"] = json!({"name":name,"id":container});
    let mut status = match docker.text(&["start", &container]) {
        Ok(_) => Status::Pass,
        Err(e) => run.stop(Stop::error("container", "container_start_failed", e)),
    };
    drop(startup_lock);
    run.report["isolation"] = isolation(docker, &container, args);
    if run.report["isolation"]["ok"] != true {
        status = report::overall([
            status,
            run.stop(Stop::error(
                "isolation",
                "isolation_violation",
                run.report["isolation"].to_string(),
            )),
        ]);
    }
    if status == Status::Pass {
        if args.vnc {
            log.line(&format!(
                "VNC: vnc://127.0.0.1:{} (только локально)",
                args.vnc_port
            ));
        }
        let mut argv = vec![
            "anki-e2e".to_owned(),
            "agent".into(),
            "--scenarios".into(),
            scenarios.join(","),
            "--expect-anki-version".into(),
            resolved.version.clone(),
        ];
        if args.vnc {
            argv.push("--vnc".into());
        }
        log.line(&format!(
            "Исполнение сценариев в контейнере {name}: {}",
            scenarios.join(", ")
        ));
        let deadline = Instant::now() + Duration::from_secs(args.timeout_minutes * 60);
        let end = match Exec::spawn(docker, &container, &argv) {
            Ok(exec) => {
                let printer = log.clone();
                Some(exec.wait(
                    deadline,
                    &|| interrupted.load(Ordering::SeqCst),
                    &move |line, _| printer.line(&format!("  {line}")),
                ))
            }
            Err(e) => {
                status = run.stop(Stop::error("agent", "agent_exec_failed", e));
                None
            }
        };
        let agent_dir = run.dir.join("agent");
        let copied = docker.text(&[
            "cp",
            &format!("{container}:/work/out"),
            &agent_dir.to_string_lossy(),
        ]);
        run.report["paths"]["agent_dir"] = json!(agent_dir);
        let agent = util::read_json(&agent_dir.join("agent-result.json")).unwrap_or(Value::Null);
        status = interpret_agent(run, status, end, &agent, &copied, args.timeout_minutes);
    }
    let keep = match args.keep_container {
        Keep::Never => false,
        Keep::Always => true,
        Keep::OnFailure => status != Status::Pass,
    };
    if keep {
        log.line(&format!(
            "Контейнер сохранён для диагностики: {name}. Удаление: cargo run -p anki-e2e -- clean"
        ));
        run.report["cleanup"]["container"] =
            json!({"preserved":true,"name":name,"reason":"--keep-container"});
        return status;
    }
    let removed = docker.remove_container(&container);
    let gone = !docker.container_exists(&container);
    run.report["cleanup"]["container"] =
        json!({"removed":removed.is_ok() && gone,"error":removed.err()});
    if !gone {
        return report::overall([
            status,
            run.stop(Stop::error(
                "cleanup",
                "container_cleanup_failed",
                format!("Контейнер {name} не удалён"),
            )),
        ]);
    }
    log.line(&format!("Контейнер {name} удалён"));
    status
}

/// Совпадение хэша дерева, с которым собран бинарник агента, со снимком.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Provenance {
    Confirmed,
    Mismatch(String),
    /// Агент не сообщил хэш: несовпадение не доказано, но и PASS не подтверждён.
    Unconfirmed,
}

fn check_provenance(expected: &Value, build: &Value) -> Provenance {
    match (expected.as_str(), build["source_tree_sha256"].as_str()) {
        (Some(expected), Some(actual)) if expected == actual => Provenance::Confirmed,
        (Some(_), Some(actual)) => Provenance::Mismatch(actual.to_owned()),
        _ => Provenance::Unconfirmed,
    }
}

/// Итог агента: достоверное несовпадение происхождения — первичная причина;
/// иначе первична остановка агента, а сбор свидетельств и отсутствие
/// подтверждения происхождения не дают PASS, но её не затирают.
fn interpret_agent(
    run: &mut Run,
    mut status: Status,
    end: Option<ExecEnd>,
    agent: &Value,
    copied: &Result<String, String>,
    timeout_minutes: u64,
) -> Status {
    let provenance = check_provenance(&run.report["source"]["tree_sha256"], &agent["build"]);
    run.report["provenance"] = json!({
        "expected_source_tree_sha256": run.report["source"]["tree_sha256"],
        "agent_source_tree_sha256": agent["build"]["source_tree_sha256"],
        "state": match &provenance {
            Provenance::Confirmed => "confirmed",
            Provenance::Mismatch(_) => "mismatch",
            Provenance::Unconfirmed => "unconfirmed",
        },
    });
    if let Provenance::Mismatch(actual) = &provenance {
        status = report::overall([
            status,
            run.stop(Stop::error(
                "agent",
                "provenance_mismatch",
                format!(
                    "Бинарник в контейнере собран из дерева {actual}, а не из проверяемого снимка {}",
                    run.report["source"]["tree_sha256"]
                ),
            )),
        ]);
    }
    if let Some(end) = end {
        let agent_status = match end {
            ExecEnd::TimedOut => run.stop(Stop::error(
                "agent",
                "run_timeout",
                format!("Сценарии не завершились за {timeout_minutes} мин"),
            )),
            ExecEnd::Interrupted => run.stop(Stop {
                status: Status::Interrupted,
                stage: "agent",
                reason: "interrupted".into(),
                message: "Прервано пользователем; контейнер будет удалён".into(),
            }),
            ExecEnd::Exited(code) => match agent["status"].as_str().and_then(Status::parse) {
                Some(s) => {
                    if let Some(stop) = agent.get("stop").filter(|v| !v.is_null()) {
                        run.log.line(&format!(
                            "{} [agent] {}: {}",
                            s.as_str(),
                            stop["reason"].as_str().unwrap_or(""),
                            stop["message"].as_str().unwrap_or("")
                        ));
                        run.record_stop(json!({"status":s,"stage":"agent","reason":stop["reason"],"message":stop["message"]}));
                    }
                    s
                }
                None => match copied {
                    Err(e) => run.stop(Stop::error(
                        "evidence",
                        "evidence_copy_failed",
                        format!(
                            "Агент завершился с кодом {code}; свидетельства не скопированы: {e}"
                        ),
                    )),
                    Ok(_) => run.stop(Stop::error(
                        "agent",
                        "agent_result_missing",
                        format!("Агент завершился с кодом {code}; agent-result.json не получен"),
                    )),
                },
            },
        };
        status = report::overall([status, agent_status]);
    }
    if let Err(e) = copied
        && agent.is_object()
    {
        status = report::overall([
            status,
            run.stop(Stop::error(
                "evidence",
                "evidence_copy_failed",
                format!("Свидетельства агента скопированы не полностью: {e}"),
            )),
        ]);
    }
    if provenance == Provenance::Unconfirmed && status == Status::Pass {
        status = run.stop(Stop::error(
            "agent",
            "provenance_unconfirmed",
            "Агент не подтвердил хэш дерева исходников; PASS без происхождения не выдаётся",
        ));
    }
    run.report["installed_anki_version"] = agent["installed_anki_version"].clone();
    run.report["agent"] = json!({"build":agent["build"],"environment":agent["environment"]});
    run.report["scenarios"] = agent["scenarios"].clone();
    run.report["ux_matrix"] = agent["ux_matrix"].clone();
    status
}

/// Доказательство границ: нет монтирований, привилегий и опубликованных портов.
fn isolation(docker: &Docker, container: &str, args: &RunArgs) -> Value {
    let inspect = docker
        .json(&["container", "inspect", container])
        .unwrap_or(Value::Null);
    let c = &inspect[0];
    let host = &c["HostConfig"];
    let mounts = c["Mounts"].as_array().cloned().unwrap_or_default();
    let ports = host["PortBindings"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let ports_ok = ports.iter().all(|(port, bindings)| {
        args.vnc
            && port == "5900/tcp"
            && bindings
                .as_array()
                .is_some_and(|b| b.iter().all(|x| x["HostIp"] == "127.0.0.1"))
    });
    let cap_drop_all = host["CapDrop"]
        .as_array()
        .is_some_and(|c| c.iter().any(|v| v == "ALL"));
    let checks = json!({
        "no_mounts": mounts.is_empty() && host["Binds"].is_null(),
        "not_privileged": host["Privileged"] == false,
        "no_added_capabilities": host["CapAdd"].is_null() || host["CapAdd"].as_array().is_some_and(|a| a.is_empty()),
        "all_capabilities_dropped": cap_drop_all,
        "no_new_privileges": host["SecurityOpt"].as_array().is_some_and(|o| o.iter().any(|v| v == "no-new-privileges")),
        "non_root_user": c["Config"]["User"] == "10001:10001",
        "no_host_network_pid_ipc": host["NetworkMode"] != "host" && host["PidMode"].as_str().unwrap_or("").is_empty() && host["IpcMode"] != "host",
        "ports_only_explicit_local_vnc": ports_ok,
        "no_devices": host["Devices"].is_null() || host["Devices"].as_array().is_some_and(|d| d.is_empty()),
    });
    let ok = checks
        .as_object()
        .is_some_and(|m| m.values().all(|v| v == true));
    json!({"ok":ok,"checks":checks,"mounts":mounts,"port_bindings":ports,"network_mode":host["NetworkMode"],
        "memory":host["Memory"],"nano_cpus":host["NanoCpus"],"pids_limit":host["PidsLimit"],"user":c["Config"]["User"]})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_lock_excludes_another_open_file_until_drop() {
        let dir = std::env::temp_dir().join(format!(
            "anki-e2e-lock-test-{}",
            util::random_hex(8).unwrap()
        ));
        let path = dir.join("runner.lock");
        let first = RunnerLock::acquire_at(&path, &|| false).unwrap();
        let second = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();

        assert!(second.try_lock().is_err());
        drop(first);
        assert!(second.try_lock().is_ok());
        drop(second);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn runner_lock_wait_stops_when_interrupted() {
        let dir = std::env::temp_dir().join(format!(
            "anki-e2e-lock-interrupt-test-{}",
            util::random_hex(8).unwrap()
        ));
        let path = dir.join("runner.lock");
        let first = RunnerLock::acquire_at(&path, &|| false).unwrap();
        let interrupted = Arc::new(AtomicBool::new(false));
        let worker_flag = interrupted.clone();
        let worker_path = path.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result =
                RunnerLock::acquire_at(&worker_path, &|| worker_flag.load(Ordering::SeqCst))
                    .map(|_| ())
                    .map_err(|error| error.kind());
            sender.send(result).unwrap();
        });

        std::thread::sleep(Duration::from_millis(50));
        interrupted.store(true, Ordering::SeqCst);
        let result = receiver.recv_timeout(Duration::from_secs(2));
        drop(first);
        worker.join().unwrap();

        assert_eq!(result.unwrap(), Err(io::ErrorKind::Interrupted));
        fs::remove_dir_all(dir).unwrap();
    }

    fn identity() -> HostIdentity {
        HostIdentity {
            machine: "machine".into(),
            boot: "boot".into(),
        }
    }

    fn labels(pid: u32, runner: Option<&str>) -> String {
        let mut labels = format!("{LABEL}=1,{LABEL_PID}={pid}");
        if let Some(runner) = runner {
            labels.push_str(&format!(",{LABEL_RUNNER}={runner}"));
        }
        labels
    }

    #[test]
    fn reused_runner_pid_is_not_proof_of_ownership() {
        let local = identity();
        let started_at_100 = |pid: u32| (pid == 42).then_some(100);
        let anything = |_: u32| Some("bash".to_owned());
        let alive = labels(42, Some("machine:boot:100"));
        assert_eq!(
            runner_state(&alive, Some(&local), &started_at_100, &anything),
            RunnerState::Alive
        );
        // Тот же PID у процесса с другим временем старта — PID выдан повторно.
        let reused = labels(42, Some("machine:boot:99"));
        assert_eq!(
            runner_state(&reused, Some(&local), &started_at_100, &anything),
            RunnerState::Stale
        );
        // Контейнер прошлой загрузки ядра и отсутствующий процесс.
        assert_eq!(
            runner_state(
                &labels(42, Some("machine:old-boot:100")),
                Some(&local),
                &started_at_100,
                &anything
            ),
            RunnerState::Stale
        );
        assert_eq!(
            runner_state(
                &labels(7, Some("machine:boot:100")),
                Some(&local),
                &started_at_100,
                &anything
            ),
            RunnerState::Stale
        );
        // Прогон другого хоста отсюда не проверить и не удалить.
        assert_eq!(
            runner_state(
                &labels(42, Some("other:boot:100")),
                Some(&local),
                &started_at_100,
                &anything
            ),
            RunnerState::Foreign
        );
        assert_eq!(
            runner_state(&alive, None, &started_at_100, &anything),
            RunnerState::Alive
        );
    }

    #[test]
    fn legacy_container_without_identity_needs_live_anki_e2e_process() {
        let local = identity();
        let exists = |_: u32| Some(1);
        let missing = |_: u32| None;
        let e2e = |_: u32| Some("anki-e2e".to_owned());
        let other = |_: u32| Some("postgres".to_owned());
        let legacy = labels(42, None);
        assert_eq!(
            runner_state(&legacy, Some(&local), &exists, &e2e),
            RunnerState::Alive
        );
        assert_eq!(
            runner_state(&legacy, Some(&local), &exists, &other),
            RunnerState::Stale
        );
        assert_eq!(
            runner_state(&legacy, Some(&local), &missing, &e2e),
            RunnerState::Stale
        );
        assert_eq!(
            runner_state("", Some(&local), &exists, &e2e),
            RunnerState::Stale
        );
    }

    #[test]
    fn current_process_label_identifies_itself_as_alive() {
        let Some(label) = runner_label() else {
            return;
        };
        let local = HostIdentity::current();
        let own = labels(std::process::id(), Some(&label));
        assert_eq!(local_runner_state(&own, local.as_ref()), RunnerState::Alive);
        let stale = labels(4_294_967_295, Some(&label));
        assert_eq!(
            local_runner_state(&stale, local.as_ref()),
            RunnerState::Stale
        );
    }

    fn test_run(name: &str) -> (Run, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "anki-e2e-host-{name}-{}",
            util::random_hex(6).unwrap()
        ));
        fs::create_dir_all(&dir).unwrap();
        let run = Run {
            id: "test".into(),
            dir: dir.clone(),
            log: Arc::new(Log {
                file: Mutex::new(None),
            }),
            report: json!({"source":{"tree_sha256":"expected"}}),
            started: util::unix_now(),
        };
        (run, dir)
    }

    #[test]
    fn early_agent_failure_without_build_keeps_primary_reason() {
        let (mut run, dir) = test_run("early");
        for reason in [
            "installed_version_mismatch",
            "environment_not_ready",
            "service_spawn_failed",
        ] {
            run.report = json!({"source":{"tree_sha256":"expected"}});
            let agent = json!({"status":"ERROR","stop":{"reason":reason,"message":"ранний отказ"},"scenarios":[]});
            let status = interpret_agent(
                &mut run,
                Status::Pass,
                Some(ExecEnd::Exited(3)),
                &agent,
                &Ok(String::new()),
                120,
            );
            assert_eq!(status, Status::Error);
            assert_eq!(run.report["stop"]["reason"], reason);
            assert_eq!(run.report["provenance"]["state"], "unconfirmed");
            assert!(run.report["secondary_stops"].is_null());
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn real_provenance_mismatch_is_primary_and_pass_needs_confirmation() {
        let (mut run, dir) = test_run("provenance");
        let agent = json!({"status":"FAIL","stop":{"reason":"wait_timeout","message":"m"},
            "build":{"source_tree_sha256":"other"},"scenarios":[]});
        let status = interpret_agent(
            &mut run,
            Status::Pass,
            Some(ExecEnd::Exited(1)),
            &agent,
            &Ok(String::new()),
            120,
        );
        assert_eq!(status, Status::Error);
        assert_eq!(run.report["stop"]["reason"], "provenance_mismatch");
        assert_eq!(run.report["secondary_stops"][0]["reason"], "wait_timeout");

        run.report = json!({"source":{"tree_sha256":"expected"}});
        let confirmed =
            json!({"status":"PASS","build":{"source_tree_sha256":"expected"},"scenarios":[]});
        let status = interpret_agent(
            &mut run,
            Status::Pass,
            Some(ExecEnd::Exited(0)),
            &confirmed,
            &Ok(String::new()),
            120,
        );
        assert_eq!(status, Status::Pass);
        assert!(run.report["stop"].is_null());

        run.report = json!({"source":{"tree_sha256":"expected"}});
        let without_build = json!({"status":"PASS","scenarios":[]});
        let status = interpret_agent(
            &mut run,
            Status::Pass,
            Some(ExecEnd::Exited(0)),
            &without_build,
            &Ok(String::new()),
            120,
        );
        assert_eq!(status, Status::Error);
        assert_eq!(run.report["stop"]["reason"], "provenance_unconfirmed");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn evidence_copy_failure_is_reported_without_masking_agent_failure() {
        let (mut run, dir) = test_run("copy");
        let status = interpret_agent(
            &mut run,
            Status::Pass,
            Some(ExecEnd::Exited(3)),
            &Value::Null,
            &Err("docker cp: no space left".into()),
            120,
        );
        assert_eq!(status, Status::Error);
        assert_eq!(run.report["stop"]["reason"], "evidence_copy_failed");

        run.report = json!({"source":{"tree_sha256":"expected"}});
        let failed = json!({"status":"FAIL","stop":{"reason":"unknown_blocking_dialog","message":"m"},
            "build":{"source_tree_sha256":"expected"},"scenarios":[]});
        let status = interpret_agent(
            &mut run,
            Status::Pass,
            Some(ExecEnd::Exited(1)),
            &failed,
            &Err("docker cp: partial".into()),
            120,
        );
        assert_eq!(status, Status::Error);
        assert_eq!(run.report["stop"]["reason"], "unknown_blocking_dialog");
        assert_eq!(
            run.report["secondary_stops"][0]["reason"],
            "evidence_copy_failed"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unwritable_report_turns_pass_into_error_and_keeps_failure_code() {
        let (mut run, dir) = test_run("report");
        let blocker = dir.join("not-a-directory");
        fs::write(&blocker, b"file").unwrap();
        run.dir = blocker.join("run");
        assert_eq!(finish(&mut run, Status::Pass), Status::Error.exit_code());
        assert_eq!(run.report["status"], "ERROR");
        assert_eq!(
            run.report["report_write_errors"].as_array().unwrap().len(),
            2
        );
        run.report = json!({"stop":{"reason":"wait_timeout"}});
        assert_eq!(finish(&mut run, Status::Fail), Status::Fail.exit_code());

        run.dir = dir.join("ok");
        run.report = json!({});
        assert_eq!(finish(&mut run, Status::Pass), 0);
        assert!(run.dir.join("report.json").is_file() && run.dir.join("report.md").is_file());
        fs::remove_dir_all(dir).unwrap();
    }
}
