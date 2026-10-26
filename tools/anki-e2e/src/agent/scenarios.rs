//! Сценарии E2E. Каждый работает со своей пустой базой Anki и проходит через
//! настоящий `anki-repo anki`, штатные окна Anki и установленные аддоны.
//! Результат подтверждается независимо: upstream AnkiConnect, CrowdAnki JSON,
//! файлы на диске и наблюдаемые окна.

use super::Fail;
use super::ctx::{CLI_POLL, Ctx, anki_processes, signal_anki};
use super::fixtures::{self, CONNECT, CROWD, DECK, MEDIA, MODEL, NOTES};
use super::ui::Policy;
use base64::Engine as _;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub type Outcome = Result<(), Fail>;

/// Фикстуры прогона: создаются настоящими шагами и копируются только при
/// остановленной Anki; сценарий никогда не изменяет саму фикстуру.
pub struct Fixtures {
    root: PathBuf,
    addons: Option<PathBuf>,
    patched: Option<PathBuf>,
    pub expected_version: Option<String>,
}

impl Fixtures {
    pub fn new(root: PathBuf, expected_version: Option<String>) -> Fixtures {
        Fixtures {
            root,
            addons: None,
            patched: None,
            expected_version,
        }
    }

    fn store(&self, c: &Ctx, name: &str) -> Result<PathBuf, Fail> {
        if !anki_processes().is_empty() {
            return Err(Fail::error(
                "fixture_while_running",
                "Фикстура снимается только при остановленной Anki",
            ));
        }
        let dest = self.root.join(name);
        if dest.exists() {
            fs::remove_dir_all(&dest).map_err(|e| Fail::error("fixture_io", e.to_string()))?;
        }
        fixtures::copy_tree(&c.base, &dest)?;
        Ok(dest)
    }

    fn restore_into(c: &mut Ctx, from: &Path, label: &str) -> Outcome {
        fixtures::copy_tree(from, &c.base)?;
        c.note(format!(
            "База создана из фикстуры «{label}», полученной настоящими шагами этого прогона"
        ));
        Ok(())
    }

    /// Чистый профиль + оба аддона, установленные через `setup --apply`.
    pub fn base_with_addons(&mut self, c: &mut Ctx) -> Outcome {
        let step = c.ui.step.clone();
        if self.addons.is_none() {
            c.step("fixture: первый запуск");
            first_run(c)?;
            install_both_by_rerun(c)?;
            self.addons = Some(self.store(c, "addons")?);
            fs::remove_dir_all(&c.base).map_err(|e| Fail::error("fixture_io", e.to_string()))?;
        }
        let from = self.addons.clone().expect("fixture");
        c.step(&step);
        Self::restore_into(c, &from, "аддоны установлены")
    }

    /// Плюс установленный патч (`patch install --apply`).
    pub fn base_patched(&mut self, c: &mut Ctx) -> Outcome {
        let step = c.ui.step.clone();
        if self.patched.is_none() {
            self.base_with_addons(c)?;
            c.step("fixture: patch install");
            let r = c.anki_repo(&["patch", "install", "--apply"], &Policy::none())?;
            c.check(
                "fixture: патч установлен",
                r.state() == "pending_restart",
                r.json.clone(),
            )?;
            self.patched = Some(self.store(c, "patched")?);
            fs::remove_dir_all(&c.base).map_err(|e| Fail::error("fixture_io", e.to_string()))?;
        }
        let from = self.patched.clone().expect("fixture");
        c.step(&step);
        Self::restore_into(c, &from, "аддоны и патч установлены")
    }

    pub fn remember_addons(&mut self, c: &Ctx) -> Outcome {
        self.addons = Some(self.store(c, "addons")?);
        Ok(())
    }
}

fn first_run_policy() -> Policy {
    Policy::none()
        .click("anki_first_run_language", "OK", 1)
        .click("anki_first_run_language_confirm", "Yes", 1)
}

fn install_policy() -> Policy {
    Policy::none()
        .click("anki_addon_install_warning", "Yes", 1)
        .click("anki_addon_installation_complete", "OK", 1)
}

/// Настоящий первый запуск пустой базы: выбор языка и профиль «User 1».
pub fn first_run(c: &mut Ctx) -> Outcome {
    c.spawn_anki(&[], None)?;
    let policy = first_run_policy();
    c.wait_main_window(&policy)?;
    let selected = c.ui.events.iter().any(|e| {
        e["rule"] == "anki_first_run_language"
            && e["selected_items"]
                .as_array()
                .is_some_and(|s| s.iter().any(|i| i == "English (United States)"))
    });
    c.check(
        "Первый запуск: выбран English (United States) до нажатия OK",
        selected,
        json!(null),
    )?;
    c.quit_via_gui(&policy)?;
    let created = c.base.join("prefs21.db").is_file() && c.base.join(super::ctx::PROFILE).is_dir();
    c.check(
        "Первый запуск создал prefs21.db и профиль User 1",
        created,
        json!(c.base),
    )
}

fn result_path(v: &Value, key: &str) -> Option<PathBuf> {
    v["result"][key].as_str().map(PathBuf::from)
}

/// `setup --apply`; отказ адаптера для этой версии Anki — BLOCKED с фактической причиной.
fn setup_apply(c: &mut Ctx, policy: &Policy) -> Result<super::ctx::Cmd, Fail> {
    let r = c.anki_repo(&["setup", "--apply"], policy)?;
    if r.error_reason() == "unsupported_anki" {
        let version = c.anki_repo(&["doctor"], &Policy::none())?.result()["version"].clone();
        return Err(Fail::blocked(
            "anki_version_unsupported_by_desktop_adapter",
            format!(
                "anki-repo anki setup отказал для фактической Anki {version}: {} (стадия setup, allowlist compatibility.json)",
                r.error_message()
            ),
        ));
    }
    Ok(r)
}

/// Установка обоих аддонов путём повторного setup после закрытия Anki.
fn install_both_by_rerun(c: &mut Ctx) -> Outcome {
    for (round, id) in [(1, CONNECT), (2, CROWD)] {
        c.step(&format!("fixture: setup --apply #{round}"));
        let policy = install_policy();
        let r = setup_apply(c, &policy)?;
        c.check(
            "setup открыл штатный установщик",
            r.state() == "awaiting_native_installer",
            r.json.clone(),
        )?;
        let step = c.ui.step.clone();
        c.wait(
            "установщик Anki и главное окно",
            Duration::from_secs(180),
            &policy,
            |c| {
                Ok(
                    c.ui.seen_in_step("anki_addon_installation_complete", &step) == 1
                        && c.ui.main_window().is_some()
                        && c.ui.dialogs().is_empty(),
                )
            },
        )?;
        c.quit_via_gui(&Policy::none())?;
        let installed = c
            .base
            .join("addons21")
            .join(id)
            .join("__init__.py")
            .is_file();
        c.check(
            &format!("Аддон {id} установлен штатным установщиком"),
            installed,
            json!(id),
        )?;
    }
    Ok(())
}

fn doctor(c: &mut Ctx, policy: &Policy) -> Result<super::ctx::Cmd, Fail> {
    c.anki_repo(&["doctor"], policy)
}

fn addon_state(d: &Value, id: &str) -> String {
    d["result"]["addons"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["id"] == id))
        .and_then(|x| x["state"].as_str())
        .unwrap_or("")
        .to_owned()
}

fn wait_live(c: &mut Ctx, state: &str) -> Result<Value, Fail> {
    let mut last = Value::Null;
    let want = state.to_owned();
    c.wait_every(
        &format!("doctor live.state={state}"),
        Duration::from_secs(90),
        &Policy::none(),
        CLI_POLL,
        |c| {
            let d = c.anki_repo(&["doctor"], &Policy::none())?;
            last = d.json.clone();
            Ok(d.result()["live"]["state"] == want.as_str())
        },
    )?;
    Ok(last)
}

fn launch_owned(c: &mut Ctx) -> Outcome {
    let l = c.anki_repo(&["launch", "--apply"], &Policy::none())?;
    c.check(
        "launch --apply запускает собственный процесс",
        l.state() == "starting",
        l.json.clone(),
    )?;
    c.wait_main_window(&Policy::none())
}

fn stop_owned(c: &mut Ctx) -> Outcome {
    let s = c.anki_repo(&["stop", "--apply"], &Policy::none())?;
    c.check(
        "stop --apply закрывает подтверждённо собственный процесс",
        s.state() == "stopped",
        s.json.clone(),
    )?;
    c.check(
        "После stop процессов Anki нет",
        anki_processes().is_empty(),
        json!(null),
    )
}

pub fn smoke(c: &mut Ctx, fx: &mut Fixtures) -> Outcome {
    c.step("первый запуск");
    first_run(c)?;
    c.step("doctor остановленной Anki");
    let d = doctor(c, &Policy::none())?;
    let r = d.result().clone();
    c.check(
        "doctor без готовности возвращает код 6",
        d.exit == 6 && r["ready"] == false,
        json!({"exit":d.exit}),
    )?;
    c.check(
        "Цель — Linux Anki",
        r["target_platform"] == "linux" && r["cli_platform"] == "linux",
        r["target_platform"].clone(),
    )?;
    if let Some(v) = fx.expected_version.clone() {
        c.check(
            &format!("doctor видит фактическую версию {v}"),
            r["version"] == v.as_str(),
            r["version"].clone(),
        )?;
    }
    c.check(
        "Оба аддона отсутствуют, патча нет",
        addon_state(&d.json, CONNECT) == "missing"
            && addon_state(&d.json, CROWD) == "missing"
            && r["patch"]["state"] == "missing",
        json!({"addons":r["addons"],"patch":r["patch"]}),
    )?;
    c.check(
        "Остановленная Anki: процессов нет, API не отвечает",
        r["running"] == false && r["live"]["state"] == "http_unreachable",
        r["live"].clone(),
    )?;
    c.step("launch");
    launch_owned(c)?;
    c.step("doctor запущенной Anki без аддонов");
    let d = doctor(c, &Policy::none())?;
    let r = d.result().clone();
    c.check(
        "Процесс обнаружен",
        r["running"] == true && r["processes"].as_array().is_some_and(|p| p.len() == 1),
        r["processes"].clone(),
    )?;
    c.check(
        "Без AnkiConnect живой API не отвечает и не выдаётся за готовность",
        r["live"]["state"] == "http_unreachable" && r["ready"] == false,
        r["live"].clone(),
    )?;
    c.step("unsafe endpoint");
    let e = c.anki_repo(
        &["--endpoint", "http://192.0.2.1:8765", "doctor"],
        &Policy::none(),
    )?;
    c.check(
        "Удалённый endpoint отвергается до запроса",
        e.result()["live"]["state"] == "unsafe_endpoint",
        e.result()["live"].clone(),
    )?;
    c.step("stop без handshake");
    let s = c.anki_repo(&["stop", "--apply"], &Policy::none())?;
    c.check(
        "Без патча владение не подтверждено: restart_required, процесс не убит",
        s.state() == "restart_required"
            && s.result()["reason"] == "user_process_not_owned"
            && !anki_processes().is_empty(),
        s.json.clone(),
    )?;
    c.step("закрытие пользователем");
    c.quit_via_gui(&Policy::none())?;
    let d = doctor(c, &Policy::none())?;
    c.check(
        "После закрытия running=false",
        d.result()["running"] == false,
        d.result()["processes"].clone(),
    )
}

pub fn addons(c: &mut Ctx, fx: &mut Fixtures) -> Outcome {
    c.step("первый запуск");
    first_run(c)?;
    c.step("setup: план");
    let p = c.anki_repo(&["setup"], &Policy::none())?;
    c.check(
        "План перечисляет оба отсутствующих аддона и AnkiWeb-источники",
        p.exit == 0
            && p.state() == "setup_plan"
            && p.result()["missing"] == json!([CONNECT, CROWD])
            && p.result()["sources"]
                .as_array()
                .is_some_and(|s| s.len() == 2),
        p.json.clone(),
    )?;
    c.check(
        "План ничего не устанавливает",
        !c.base.join("addons21").join(CONNECT).exists(),
        json!(null),
    )?;

    c.step("setup --apply: первый пакет");
    let policy = install_policy();
    let s = setup_apply(c, &policy)?;
    let r = s.result().clone();
    c.check(
        "setup --apply → awaiting_native_installer без автоматической установки",
        s.state() == "awaiting_native_installer"
            && r["automatic_installation"] == false
            && r["packages"].as_array().is_some_and(|p| p.len() == 2),
        s.json.clone(),
    )?;
    c.check(
        "Backup до изменения создан",
        result_path(&s.json, "backup").is_some_and(|b| b.join("snapshot.json").is_file()),
        r["backup"].clone(),
    )?;
    let step = c.ui.step.clone();
    c.wait(
        "подтверждение установщика, итог и главное окно",
        Duration::from_secs(180),
        &policy,
        |c| {
            Ok(
                c.ui.seen_in_step("anki_addon_installation_complete", &step) == 1
                    && c.ui.main_window().is_some()
                    && c.ui.dialogs().is_empty(),
            )
        },
    )?;
    let warned = c.ui.events.iter().any(|e| {
        e["rule"] == "anki_addon_install_warning"
            && e["step"] == step.as_str()
            && e["text"]
                .as_str()
                .is_some_and(|t| t.contains(&format!("{CONNECT}.ankiaddon")))
            && e["test_action"] == "click:Yes"
    });
    c.check(
        "awaiting_native_installer соответствует реальному диалогу, требующему решения человека",
        warned,
        json!(
            c.ui.events
                .iter()
                .filter(|e| e["step"] == step.as_str())
                .collect::<Vec<_>>()
        ),
    )?;
    let d = wait_live(c, "ankiconnect_responding")?;
    c.check(
        "AnkiConnect установлен и загружен в этом же запуске; CrowdAnki ещё нет",
        addon_state(&d, CONNECT) == "installed_unverified"
            && addon_state(&d, CROWD) == "missing"
            && d["result"]["ready"] == false,
        json!({"addons":d["result"]["addons"],"live":d["result"]["live"]}),
    )?;

    c.step("второй пакет через работающую Anki");
    let package = r["packages"][1].as_str().unwrap_or("").to_owned();
    c.check(
        "Второй пакет — CrowdAnki",
        package.ends_with(&format!("{CROWD}.ankiaddon")),
        json!(package),
    )?;
    let policy = Policy::none()
        .click("anki_addon_install_warning", "Yes", 1)
        .leave("anki_addon_installed_restart_tooltip", 1);
    c.spawn_anki(&[&package], None)?;
    let step = c.ui.step.clone();
    c.wait(
        "подсказка об установке во время работы",
        Duration::from_secs(120),
        &policy,
        |c| {
            Ok(c.ui
                .seen_in_step("anki_addon_installed_restart_tooltip", &step)
                == 1)
        },
    )?;
    c.wait(
        "второй экземпляр передал файл и завершился",
        Duration::from_secs(60),
        &policy,
        |_| Ok(anki_processes().len() == 1),
    )?;
    c.observe(Duration::from_secs(5), &policy)?;
    let d = doctor(c, &policy)?;
    c.check(
        "CrowdAnki записан на диск; до перезапуска готовность не заявлена",
        addon_state(&d.json, CROWD) == "installed_unverified" && d.result()["ready"] == false,
        d.result()["addons"].clone(),
    )?;
    c.step("закрытие");
    c.quit_via_gui(&policy)?;

    c.step("повторный setup: no-op");
    let s = c.anki_repo(&["setup", "--apply"], &Policy::none())?;
    c.check(
        "Повторный setup здоровых аддонов — no_installation_needed",
        s.state() == "no_installation_needed",
        s.json.clone(),
    )?;
    c.observe(Duration::from_secs(8), &Policy::none())?;
    c.check(
        "No-op setup не запускает Anki и не показывает «Installed»",
        anki_processes().is_empty()
            && c.ui
                .seen_in_step("anki_addon_installation_complete", &c.ui.step.clone())
                == 0,
        json!(null),
    )?;

    c.step("перезапуск: оба аддона загружаются без ошибок");
    launch_owned(c)?;
    c.observe(Duration::from_secs(5), &Policy::none())?;
    let d = wait_live(c, "ankiconnect_responding")?;
    c.check(
        "После перезапуска оба аддона на диске, без патча готовности нет",
        addon_state(&d, CONNECT) == "installed_unverified"
            && addon_state(&d, CROWD) == "installed_unverified"
            && d["result"]["patch"]["state"] == "missing",
        d["result"].clone(),
    )?;
    let s = c.anki_repo(&["stop", "--apply"], &Policy::none())?;
    c.check(
        "Без патча stop не закрывает процесс",
        s.result()["reason"] == "user_process_not_owned",
        s.json.clone(),
    )?;
    c.quit_via_gui(&Policy::none())?;
    fx.remember_addons(c)
}

fn bootstrap_present(base: &Path) -> bool {
    fs::read_to_string(base.join("addons21").join(CONNECT).join("__init__.py"))
        .is_ok_and(|s| s.contains("# BEGIN ANKI DECK TOOLKIT BOOTSTRAP"))
}

fn backup_id(v: &Value) -> String {
    result_path(v, "backup")
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

pub fn patch(c: &mut Ctx, fx: &mut Fixtures) -> Outcome {
    fx.base_with_addons(c)?;
    let init = c.base.join("addons21").join(CONNECT).join("__init__.py");
    let bridge = c
        .base
        .join("addons21")
        .join(CONNECT)
        .join("user_files/toolkit");
    let none = Policy::none();
    c.step("status: missing");
    let s = c.anki_repo(&["patch", "status"], &none)?;
    c.check(
        "patch status → missing, код 6",
        s.state() == "missing" && s.exit == 6,
        s.json.clone(),
    )?;
    c.step("install: план");
    let upstream_init = fs::read(&init).map_err(|e| Fail::error("io", e.to_string()))?;
    let p = c.anki_repo(&["patch", "install"], &none)?;
    c.check(
        "План не меняет файлы",
        p.state() == "patch_plan" && fs::read(&init).unwrap_or_default() == upstream_init,
        p.json.clone(),
    )?;
    c.step("install --apply");
    let i = c.anki_repo(&["patch", "install", "--apply"], &none)?;
    c.check(
        "install → pending_restart и backup",
        i.state() == "pending_restart" && !backup_id(&i.json).is_empty(),
        i.json.clone(),
    )?;
    c.check(
        "Bootstrap и payload на диске",
        bootstrap_present(&c.base) && bridge.join("payload.py").is_file(),
        json!(null),
    )?;
    let s = c.anki_repo(&["patch", "status"], &none)?;
    c.check(
        "status → installed, код 0",
        s.state() == "installed" && s.exit == 0,
        s.json.clone(),
    )?;
    c.step("install повторно");
    let installed = fs::read(&init).unwrap_or_default();
    let again = c.anki_repo(&["patch", "install", "--apply"], &none)?;
    c.check(
        "Повторная установка ничего не пишет",
        again.state() == "already_installed" && fs::read(&init).unwrap_or_default() == installed,
        again.json.clone(),
    )?;

    c.step("предыдущее поколение патча");
    let settings = bridge.join("settings.json");
    let mut config: Value =
        serde_json::from_slice(&fs::read(&settings).map_err(|e| Fail::error("io", e.to_string()))?)
            .map_err(|e| Fail::error("io", e.to_string()))?;
    let token = config["token"].clone();
    config["patch_version"] = json!("0.0.0-e2e");
    fs::write(&settings, serde_json::to_vec_pretty(&config).expect("json"))
        .map_err(|e| Fail::error("io", e.to_string()))?;
    let s = c.anki_repo(&["patch", "status"], &none)?;
    c.check(
        "Другое поколение патча — outdated, а не damaged",
        s.state() == "outdated" && s.exit == 6,
        s.json.clone(),
    )?;
    let up = c.anki_repo(&["patch", "install", "--apply"], &none)?;
    let upgraded: Value =
        serde_json::from_slice(&fs::read(&settings).unwrap_or_default()).unwrap_or(Value::Null);
    c.check(
        "install обновляет патч и сохраняет capability token",
        up.state() == "pending_restart" && upgraded["token"] == token && bootstrap_present(&c.base),
        up.json.clone(),
    )?;

    c.step("загрузка патча после перезапуска");
    launch_owned(c)?;
    let d = wait_live(c, "patch_loaded")?;
    let h = &d["result"]["handshake"];
    c.check(
        "Handshake: оба аддона загружены, процесс принадлежит CLI, export/import ещё не подтверждены",
        h["addons_ready"] == true && h["managed_process"] == true && d["result"]["ready"] == false,
        h.clone(),
    )?;
    c.step("изменение патча при запущенной Anki");
    let m = c.anki_repo(&["patch", "install", "--apply"], &none)?;
    c.check(
        "При запущенной Anki патч не меняется",
        m.state() == "restart_required" && m.result()["reason"] == "stop_anki_before_patch",
        m.json.clone(),
    )?;
    c.step("stop владельца");
    stop_owned(c)?;

    c.step("стёртый bootstrap");
    fs::write(&init, &upstream_init).map_err(|e| Fail::error("io", e.to_string()))?;
    let s = c.anki_repo(&["patch", "status"], &none)?;
    c.check(
        "Стёртый bootstrap при сохранённых settings → damaged",
        s.state() == "damaged" && s.exit == 6,
        s.json.clone(),
    )?;
    let r = c.anki_repo(&["patch", "repair", "--apply"], &none)?;
    c.check(
        "repair восстанавливает bootstrap",
        r.state() == "pending_restart" && bootstrap_present(&c.base),
        r.json.clone(),
    )?;

    c.step("повреждённый payload");
    let payload = bridge.join("payload.py");
    let good = fs::read(&payload).map_err(|e| Fail::error("io", e.to_string()))?;
    fs::write(
        &payload,
        [good.as_slice(), b"\n# corrupted by e2e\n"].concat(),
    )
    .map_err(|e| Fail::error("io", e.to_string()))?;
    let s = c.anki_repo(&["patch", "status"], &none)?;
    c.check(
        "Изменённый payload → damaged",
        s.state() == "damaged",
        s.json.clone(),
    )?;
    let r = c.anki_repo(&["patch", "repair", "--apply"], &none)?;
    c.check(
        "repair восстанавливает точные байты payload",
        r.state() == "pending_restart" && fs::read(&payload).ok() == Some(good),
        r.json.clone(),
    )?;

    c.step("изменённый upstream");
    let web = c.base.join("addons21").join(CONNECT).join("web.py");
    let original = fs::read(&web).map_err(|e| Fail::error("io", e.to_string()))?;
    fs::write(
        &web,
        [original.as_slice(), b"\n# changed upstream\n"].concat(),
    )
    .map_err(|e| Fail::error("io", e.to_string()))?;
    let s = c.anki_repo(&["patch", "status"], &none)?;
    c.check(
        "Неизвестный upstream → incompatible",
        s.state() == "incompatible",
        s.json.clone(),
    )?;
    let before = fs::read(&init).unwrap_or_default();
    let r = c.anki_repo(&["patch", "repair", "--apply"], &none)?;
    c.check(
        "repair не переписывает изменённый upstream",
        r.exit != 0
            && r.error_reason() == "incompatible_addon"
            && fs::read(&init).unwrap_or_default() == before,
        r.json.clone(),
    )?;
    fs::write(&web, &original).map_err(|e| Fail::error("io", e.to_string()))?;
    let s = c.anki_repo(&["patch", "status"], &none)?;
    c.check(
        "После возврата upstream — installed",
        s.state() == "installed",
        s.json.clone(),
    )?;

    c.step("remove --apply");
    let rm = c.anki_repo(&["patch", "remove", "--apply"], &none)?;
    let removed_backup = backup_id(&rm.json);
    c.check(
        "remove убирает только собственный блок и область payload",
        rm.state() == "removed"
            && !bootstrap_present(&c.base)
            && !bridge.exists()
            && !removed_backup.is_empty(),
        rm.json.clone(),
    )?;
    let s = c.anki_repo(&["patch", "status"], &none)?;
    c.check(
        "status после remove → missing",
        s.state() == "missing",
        s.json.clone(),
    )?;
    c.step("запуск без патча");
    launch_owned(c)?;
    let d = wait_live(c, "ankiconnect_responding")?;
    c.check(
        "Без патча upstream отвечает, но готовность и владение не заявлены",
        d["result"]["ready"] == false && d["result"]["live"]["patch_error"].is_object(),
        d["result"]["live"].clone(),
    )?;
    let s = c.anki_repo(&["stop", "--apply"], &none)?;
    c.check(
        "Без handshake stop отказывает",
        s.result()["reason"] == "user_process_not_owned",
        s.json.clone(),
    )?;
    c.quit_via_gui(&none)?;

    c.step("restore");
    let plan = c.anki_repo(&["restore", &removed_backup], &none)?;
    c.check(
        "restore без --apply — план",
        plan.state() == "restore_plan",
        plan.json.clone(),
    )?;
    let rs = c.anki_repo(&["restore", &removed_backup, "--apply"], &none)?;
    c.check(
        "restore --apply возвращает аддоны с патчем",
        rs.state() == "restored" && bootstrap_present(&c.base),
        rs.json.clone(),
    )?;
    let s = c.anki_repo(&["patch", "status"], &none)?;
    c.check(
        "После restore → installed",
        s.state() == "installed",
        s.json.clone(),
    )?;
    let bad = c.anki_repo(&["restore", "../../etc"], &none)?;
    c.check(
        "restore принимает только ID",
        bad.exit != 0,
        bad.json.clone(),
    )?;
    c.step("перезапуск после restore");
    launch_owned(c)?;
    wait_live(c, "patch_loaded")?;
    stop_owned(c)
}

fn create_fixture_data(c: &mut Ctx) -> Outcome {
    c.connect(
        "createModel",
        json!({"modelName":MODEL,"inOrderFields":["Front","Back"],"css":".card{font-family:sans-serif}",
            "cardTemplates":[{"Name":"Card 1","Front":"{{Front}}","Back":"{{FrontSide}}<hr id=answer>{{Back}}"}]}),
    )?;
    c.connect("createDeck", json!({"deck":DECK}))?;
    let data = base64::engine::general_purpose::STANDARD.encode(fixtures::pixel_png());
    let stored = c.connect("storeMediaFile", json!({"filename":MEDIA,"data":data}))?;
    c.check(
        "Media сохранён штатным API под ожидаемым именем",
        stored == MEDIA,
        stored.clone(),
    )?;
    for n in NOTES {
        c.connect(
            "addNote",
            json!({"note":{"deckName":DECK,"modelName":MODEL,"fields":{"Front":n.front,"Back":n.back},"tags":n.tags,
                "options":{"allowDuplicate":false}}}),
        )?;
    }
    Ok(())
}

fn notes_info(c: &mut Ctx) -> Result<Vec<Value>, Fail> {
    let ids = c.connect("findNotes", json!({"query":format!("\"deck:{DECK}\"")}))?;
    let info = c.connect("notesInfo", json!({"notes":ids}))?;
    let mut list = info.as_array().cloned().unwrap_or_default();
    list.sort_by_key(|n| {
        n["fields"]["Front"]["value"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    });
    Ok(list)
}

fn notes_match_fixture(notes: &[Value]) -> bool {
    notes.len() == NOTES.len()
        && NOTES.iter().all(|spec| {
            notes.iter().any(|n| {
                let mut tags: Vec<&str> = n["tags"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                tags.sort_unstable();
                let mut want: Vec<&str> = spec.tags.to_vec();
                want.sort_unstable();
                n["fields"]["Front"]["value"] == spec.front
                    && n["fields"]["Back"]["value"] == spec.back
                    && n["modelName"] == MODEL
                    && tags == want
            })
        })
}

fn submit(c: &mut Ctx, args: &[&str], policy: &Policy) -> Result<(super::ctx::Cmd, String), Fail> {
    let s = c.anki_repo(args, policy)?;
    let id = s.result()["job"]["id"].as_str().unwrap_or("").to_owned();
    c.check(
        "Задание принято (submitted)",
        s.state() == "submitted" && id.len() == 64,
        s.json.clone(),
    )?;
    Ok((s, id))
}

fn wait_job(c: &mut Ctx, id: &str, policy: &Policy) -> Result<Value, Fail> {
    let mut last = Value::Null;
    let id = id.to_owned();
    c.wait_every(
        &format!("завершение задания {id}"),
        Duration::from_secs(180),
        policy,
        CLI_POLL,
        |c| {
            let j = c.anki_repo(&["job", &id], policy)?;
            last = j.json.clone();
            Ok(!matches!(j.state(), "queued" | "running" | ""))
        },
    )?;
    Ok(last)
}

struct ExportDir {
    path: PathBuf,
    deck: Value,
}

fn read_export(c: &mut Ctx, job: &Value) -> Result<ExportDir, Fail> {
    let path = PathBuf::from(job["result"]["local_export_dir"].as_str().unwrap_or(""));
    let deck: Value = serde_json::from_slice(
        &fs::read(path.join("deck.json"))
            .map_err(|e| Fail::failed("export_missing", e.to_string()))?,
    )
    .map_err(|e| Fail::failed("export_invalid", e.to_string()))?;
    let copy = c.dir.join("exports").join(
        path.parent()
            .and_then(|p| p.file_name())
            .unwrap_or_default(),
    );
    fixtures::copy_tree(&path, &copy)?;
    Ok(ExportDir { path, deck })
}

fn export_notes(deck: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    fn walk(d: &Value, out: &mut Vec<Value>) {
        out.extend(d["notes"].as_array().cloned().unwrap_or_default());
        for c in d["children"].as_array().into_iter().flatten() {
            walk(c, out);
        }
    }
    walk(deck, &mut out);
    out
}

fn change_cancelled_import_field(deck: &mut Value) -> bool {
    fn first_note(deck: &mut Value) -> Option<&mut Value> {
        if deck
            .get("notes")
            .and_then(Value::as_array)
            .is_some_and(|notes| !notes.is_empty())
        {
            return deck.get_mut("notes")?.as_array_mut()?.first_mut();
        }
        deck.get_mut("children")?
            .as_array_mut()?
            .iter_mut()
            .find_map(first_note)
    }
    let Some(field) = first_note(deck)
        .and_then(|note| note.get_mut("fields"))
        .and_then(Value::as_array_mut)
        .and_then(|fields| fields.get_mut(1))
    else {
        return false;
    };
    let changed = json!("changed by cancelled import");
    if *field == changed {
        return false;
    }
    *field = changed;
    true
}

fn export_matches_fixture(deck: &Value) -> bool {
    let notes = export_notes(deck);
    let model_uuid = deck["note_models"]
        .as_array()
        .and_then(|m| m.iter().find(|m| m["name"] == MODEL))
        .and_then(|m| m["crowdanki_uuid"].as_str())
        .unwrap_or("")
        .to_owned();
    !model_uuid.is_empty()
        && notes.len() == NOTES.len()
        && NOTES.iter().all(|spec| {
            notes.iter().any(|n| {
                let mut tags: Vec<&str> = n["tags"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                tags.sort_unstable();
                let mut want: Vec<&str> = spec.tags.to_vec();
                want.sort_unstable();
                n["fields"] == json!([spec.front, spec.back])
                    && tags == want
                    && n["note_model_uuid"] == model_uuid.as_str()
                    && n["guid"].as_str().is_some_and(|g| !g.is_empty())
            })
        })
        && deck["media_files"]
            .as_array()
            .is_some_and(|m| m.iter().any(|f| f == MEDIA))
}

fn guids(deck: &Value) -> Vec<String> {
    let mut g: Vec<String> = export_notes(deck)
        .iter()
        .filter_map(|n| n["guid"].as_str().map(str::to_owned))
        .collect();
    g.sort();
    g
}

/// Производный вход для негативной проверки: копия экспорта с одним изменением.
fn derived_export(
    c: &Ctx,
    from: &Path,
    name: &str,
    change: impl FnOnce(&mut Value, &Path),
) -> Result<PathBuf, Fail> {
    let dest = c.dir.join("derived").join(name);
    fixtures::copy_tree(from, &dest)?;
    let file = dest.join("deck.json");
    let mut deck: Value =
        serde_json::from_slice(&fs::read(&file).map_err(|e| Fail::error("io", e.to_string()))?)
            .map_err(|e| Fail::error("io", e.to_string()))?;
    change(&mut deck, &dest);
    fs::write(&file, serde_json::to_vec_pretty(&deck).expect("json"))
        .map_err(|e| Fail::error("io", e.to_string()))?;
    Ok(dest)
}

pub fn crowdanki(c: &mut Ctx, fx: &mut Fixtures) -> Outcome {
    fx.base_patched(c)?;
    let none = Policy::none();
    c.step("запуск A");
    launch_owned(c)?;
    wait_live(c, "patch_loaded")?;
    c.step("тестовая колода через AnkiConnect");
    create_fixture_data(c)?;
    let source_notes = notes_info(c)?;
    c.check(
        "Заметки созданы штатным API",
        notes_match_fixture(&source_notes),
        json!(source_notes),
    )?;

    c.step("export: план");
    let plan = c.anki_repo(&["export", "--deck", DECK], &none)?;
    c.check(
        "export без --apply — план",
        plan.state() == "operation_plan",
        plan.json.clone(),
    )?;
    c.step("export --apply");
    let (_, id) = submit(c, &["export", "--deck", DECK, "--apply"], &none)?;
    let job = wait_job(c, &id, &none)?;
    c.check(
        "Экспорт CrowdAnki успешен",
        job["result"]["state"] == "succeeded",
        job.clone(),
    )?;
    let export = read_export(c, &job)?;
    c.check(
        "deck.json: поля, теги, модель и GUID совпадают с фикстурой",
        export_matches_fixture(&export.deck),
        json!(export_notes(&export.deck)),
    )?;
    let media = fs::read(export.path.join("media").join(MEDIA)).unwrap_or_default();
    c.check(
        "Физический PNG в экспорте побайтно совпадает с исходным",
        crate::util::sha256_hex(&media) == fixtures::pixel_sha256(),
        json!({"expected":fixtures::pixel_sha256(),"actual":crate::util::sha256_hex(&media)}),
    )?;
    let d = c.anki_repo(&["doctor"], &none)?;
    c.check(
        "Одного export недостаточно для ready",
        d.result()["ready"] == false && d.exit == 6,
        d.result()["handshake"]["functional_evidence"].clone(),
    )?;

    c.step("import --apply: подтверждение");
    let accept = Policy::none().click("crowdanki_import_dialog", "OK", 1);
    let (_, id) = submit(
        c,
        &["import", &export.path.to_string_lossy(), "--apply"],
        &accept,
    )?;
    let job = wait_job(c, &id, &accept)?;
    c.check(
        "Импорт через штатный диалог CrowdAnki — succeeded",
        job["result"]["state"] == "succeeded",
        job.clone(),
    )?;
    let step = c.ui.step.clone();
    c.check(
        "Диалог импорта был показан и подтверждён",
        c.ui.seen_in_step("crowdanki_import_dialog", &step) == 1,
        json!(null),
    )?;
    let after = notes_info(c)?;
    c.check(
        "Повторный импорт того же экспорта не меняет содержимое",
        notes_match_fixture(&after),
        json!(after),
    )?;
    let d = c.anki_repo(&["doctor"], &none)?;
    c.check(
        "После export и import в этом процессе — ready, код 0",
        d.result()["ready"] == true && d.exit == 0 && d.state() == "ready",
        d.result()["handshake"].clone(),
    )?;

    c.step("import: отмена");
    let before = notes_info(c)?;
    let mut applied = false;
    let changed = derived_export(c, &export.path, "changed-field", |deck, _| {
        applied = change_cancelled_import_field(deck);
    })?;
    c.check(
        "Вход отменяемого импорта действительно изменён",
        applied,
        json!({"path": changed}),
    )?;
    let cancel = Policy::none().click("crowdanki_import_dialog", "Cancel", 1);
    let (_, id) = submit(
        c,
        &["import", &changed.to_string_lossy(), "--apply"],
        &cancel,
    )?;
    let job = wait_job(c, &id, &cancel)?;
    c.check(
        "Отмена диалога → задание cancelled, а не успех",
        job["result"]["state"] == "cancelled" && job["result"]["ready"] == false,
        job.clone(),
    )?;
    let after = notes_info(c)?;
    c.check(
        "После отмены коллекция не изменилась",
        after == before,
        json!({"before":before,"after":after}),
    )?;

    c.step("предвалидация: отсутствующий media");
    let missing = derived_export(c, &export.path, "missing-media", |_, dir| {
        let _ = fs::remove_file(dir.join("media").join(MEDIA));
    })?;
    let (_, id) = submit(c, &["import", &missing.to_string_lossy(), "--apply"], &none)?;
    let job = wait_job(c, &id, &none)?;
    c.check(
        "Отсутствующий media отклонён до upstream-импорта без диалога",
        job["result"]["state"] == "failed"
            && job["result"]["job"]["error"]
                .as_str()
                .is_some_and(|e| e.contains("missing_media"))
            && job["result"]["job"]["partial_changes_possible"] == false,
        job.clone(),
    )?;
    c.step("предвалидация: конфликт модели");
    let conflict = derived_export(c, &export.path, "model-conflict", |deck, _| {
        for m in deck["note_models"].as_array_mut().into_iter().flatten() {
            m["crowdanki_uuid"] = json!("00000000-0000-4000-8000-00000000e2e0");
        }
        for n in deck["notes"].as_array_mut().into_iter().flatten() {
            n["note_model_uuid"] = json!("00000000-0000-4000-8000-00000000e2e0");
        }
    })?;
    let (_, id) = submit(
        c,
        &["import", &conflict.to_string_lossy(), "--apply"],
        &none,
    )?;
    let job = wait_job(c, &id, &none)?;
    c.check(
        "Модель с тем же именем и другой идентичностью не импортируется автоматически",
        job["result"]["state"] == "failed"
            && job["result"]["job"]["error"]
                .as_str()
                .is_some_and(|e| e.contains("model_conflict_requires_manual_import")),
        job.clone(),
    )?;
    c.step("предвалидация: миграция модели");
    let migration = derived_export(c, &export.path, "model-migration", |deck, _| {
        for m in deck["note_models"].as_array_mut().into_iter().flatten() {
            if let Some(flds) = m["flds"].as_array_mut() {
                let mut extra = flds[0].clone();
                extra["name"] = json!("Extra");
                extra["ord"] = json!(flds.len());
                flds.push(extra);
            }
        }
        for n in deck["notes"].as_array_mut().into_iter().flatten() {
            if let Some(f) = n["fields"].as_array_mut() {
                f.push(json!(""));
            }
        }
    })?;
    let (_, id) = submit(
        c,
        &["import", &migration.to_string_lossy(), "--apply"],
        &none,
    )?;
    let job = wait_job(c, &id, &none)?;
    c.check(
        "Структурная миграция модели не проходит как безопасная",
        job["result"]["state"] == "failed"
            && job["result"]["job"]["error"]
                .as_str()
                .is_some_and(|e| e.contains("model_conversion_requires_manual_import")),
        job.clone(),
    )?;
    let after = notes_info(c)?;
    c.check(
        "Отклонённые импорты не изменили коллекцию",
        after == before,
        json!(after),
    )?;
    c.step("неизвестное задание");
    let unknown = "e2e0".repeat(16);
    let j = c.anki_repo(&["job", &unknown], &none)?;
    c.check(
        "Неизвестный job ID не выдаётся за успех",
        j.exit != 0 && j.json["error"].is_object(),
        j.json.clone(),
    )?;
    c.step("остановка A");
    stop_owned(c)?;

    c.step("база B: импорт в чистую коллекцию");
    let base_a = c.base.clone();
    let base_b = c.base.with_file_name("Anki2-B");
    c.base = base_b.clone();
    fx.base_patched(c)?;
    launch_owned(c)?;
    wait_live(c, "patch_loaded")?;
    let accept = Policy::none().click("crowdanki_import_dialog", "OK", 1);
    let (_, id) = submit(
        c,
        &["import", &export.path.to_string_lossy(), "--apply"],
        &accept,
    )?;
    let job = wait_job(c, &id, &accept)?;
    c.check(
        "Импорт в чистую базу B — succeeded",
        job["result"]["state"] == "succeeded",
        job.clone(),
    )?;
    let notes_b = notes_info(c)?;
    c.check(
        "В B те же поля, теги и модель",
        notes_match_fixture(&notes_b),
        json!(notes_b),
    )?;
    let media_b = c.connect("retrieveMediaFile", json!({"filename":MEDIA}))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(media_b.as_str().unwrap_or(""))
        .unwrap_or_default();
    c.check(
        "PNG в медиатеке B побайтно совпадает с исходным",
        crate::util::sha256_hex(&bytes) == fixtures::pixel_sha256(),
        json!({"actual":crate::util::sha256_hex(&bytes)}),
    )?;
    c.step("экспорт из B: сохранение GUID");
    let (_, id) = submit(c, &["export", "--deck", DECK, "--apply"], &none)?;
    let job = wait_job(c, &id, &none)?;
    let export_b = read_export(c, &job)?;
    c.check(
        "GUID заметок сохранились при переносе A → B",
        guids(&export_b.deck) == guids(&export.deck) && export_matches_fixture(&export_b.deck),
        json!({"a":guids(&export.deck),"b":guids(&export_b.deck)}),
    )?;
    stop_owned(c)?;
    c.base = base_a;
    Ok(())
}

pub fn process(c: &mut Ctx, fx: &mut Fixtures) -> Outcome {
    fx.base_patched(c)?;
    let none = Policy::none();
    c.step("launch");
    launch_owned(c)?;
    let d = wait_live(c, "patch_loaded")?;
    let first_process = d["result"]["handshake"]["process"].clone();
    c.step("повторный launch");
    let l = c.anki_repo(&["launch", "--apply"], &none)?;
    c.check(
        "Второй launch при работающей Anki отклонён",
        l.state() == "restart_required",
        l.json.clone(),
    )?;
    c.check(
        "Второй экземпляр не запущен",
        anki_processes().len() == 1,
        json!(anki_processes().len()),
    )?;
    c.step("restart");
    let r = c.anki_repo(&["restart", "--apply"], &none)?;
    c.check(
        "restart → starting",
        r.state() == "starting",
        r.json.clone(),
    )?;
    c.wait_main_window(&none)?;
    let d = wait_live(c, "patch_loaded")?;
    c.check(
        "После restart — новый процесс того же владельца",
        d["result"]["handshake"]["process"] != first_process
            && d["result"]["handshake"]["managed_process"] == true,
        d["result"]["handshake"].clone(),
    )?;
    c.step("stop при открытом модальном диалоге");
    create_fixture_data(c)?;
    let (_, id) = submit(c, &["export", "--deck", DECK, "--apply"], &none)?;
    let job = wait_job(c, &id, &none)?;
    let export = PathBuf::from(job["result"]["local_export_dir"].as_str().unwrap_or(""));
    let hold = Policy::none().leave("crowdanki_import_dialog", 1);
    let (_, import_id) = submit(c, &["import", &export.to_string_lossy(), "--apply"], &hold)?;
    let step = c.ui.step.clone();
    c.wait(
        "модальный диалог импорта",
        Duration::from_secs(60),
        &hold,
        |c| Ok(c.ui.seen_in_step("crowdanki_import_dialog", &step) == 1),
    )?;
    let s = c.anki_repo(&["stop", "--apply"], &hold)?;
    c.check(
        "stop при модальном окне отказывает и не закрывает Anki",
        s.exit != 0
            && s.error_message().contains("restart_required")
            && anki_processes().len() == 1,
        s.json.clone(),
    )?;
    let busy = c.anki_repo(&["export", "--deck", DECK, "--apply"], &hold)?;
    c.check(
        "Во время операции новые задания отклоняются (busy)",
        busy.exit != 0,
        busy.json.clone(),
    )?;
    c.step("отмена удерживаемого диалога");
    // Окно уже показано и учтено: Cancel нажимается явно после сверки с каталогом.
    let cancel = Policy::none().leave("crowdanki_import_dialog", 1);
    cancel_open_dialog(c)?;
    let job = wait_job(c, &import_id, &cancel)?;
    c.check(
        "Удерживаемый импорт после Cancel — cancelled",
        job["result"]["state"] == "cancelled",
        job.clone(),
    )?;

    c.step("аварийное завершение Anki");
    let killed = signal_anki(true);
    c.wait_no_anki("Anki завершена SIGKILL", Duration::from_secs(30), &none)?;
    c.note(format!(
        "SIGKILL процессов {killed:?} — моделируемая смерть Anki"
    ));
    let d = c.anki_repo(&["doctor"], &none)?;
    c.check(
        "После смерти процесса: running=false, API недоступен, готовность не заявлена",
        d.result()["running"] == false
            && d.result()["live"]["state"] == "http_unreachable"
            && d.result()["ready"] == false,
        d.result()["live"].clone(),
    )?;
    let j = c.anki_repo(&["job", &import_id], &none)?;
    c.check(
        "Задание после смерти процесса не выдаётся за успех",
        j.exit != 0 && j.state() != "succeeded",
        j.json.clone(),
    )?;
    c.step("чужой процесс");
    c.spawn_anki(&["-p", super::ctx::PROFILE], None)?;
    c.wait_main_window(&none)?;
    wait_live(c, "patch_loaded")?;
    let s = c.anki_repo(&["stop", "--apply"], &none)?;
    c.check(
        "Процесс без секрета владения не закрывается CLI",
        s.state() == "restart_required"
            && s.result()["reason"] == "user_process_not_owned"
            && anki_processes().len() == 1,
        s.json.clone(),
    )?;
    c.quit_via_gui(&none)
}

/// Нажать Cancel в уже показанном диалоге импорта, сверив его с каталогом.
fn cancel_open_dialog(c: &mut Ctx) -> Outcome {
    let dialog =
        c.ui.dialogs()
            .into_iter()
            .find(|o| {
                super::catalog::find(&o.role, &o.title, &o.text, &o.button_names())
                    .is_some_and(|r| r.id == "crowdanki_import_dialog")
            })
            .cloned()
            .ok_or_else(|| Fail::failed("dialog_missing", "Диалог импорта не найден"))?;
    let button = dialog
        .buttons
        .iter()
        .find(|b| b.name == "Cancel" && b.enabled)
        .and_then(|b| b.rect)
        .ok_or_else(|| Fail::failed("expected_button_missing", "Нет Cancel"))?;
    c.shot("before-click-Cancel");
    let (x, y) = button.center();
    c.ui.display
        .click(x, y)
        .map_err(|e| Fail::error("gui_input_failed", e))?;
    c.ui.events.push(json!({"event":"action","step":c.ui.step,"key":dialog.key,"title":dialog.title,"action":"click:Cancel"}));
    if let Some(e) =
        c.ui.events
            .iter_mut()
            .rev()
            .find(|e| e["event"] == "appeared" && e["key"] == dialog.key.as_str())
    {
        e["test_action"] = json!("hold, затем click:Cancel");
    }
    Ok(())
}

pub fn errors(c: &mut Ctx, fx: &mut Fixtures) -> Outcome {
    let none = Policy::none();
    c.step("отказ в установщике");
    first_run(c)?;
    let refuse = Policy::none().click("anki_addon_install_warning", "No", 1);
    let s = setup_apply(c, &refuse)?;
    c.check(
        "setup передал пакет установщику",
        s.state() == "awaiting_native_installer",
        s.json.clone(),
    )?;
    let step = c.ui.step.clone();
    c.wait(
        "отказ и главное окно",
        Duration::from_secs(180),
        &refuse,
        |c| {
            Ok(c.ui.seen_in_step("anki_addon_install_warning", &step) == 1
                && c.ui.main_window().is_some()
                && c.ui.dialogs().is_empty())
        },
    )?;
    c.check(
        "После No нет сообщения об успешной установке",
        c.ui.seen("anki_addon_installation_complete") == 0,
        json!(null),
    )?;
    let d = c.anki_repo(&["doctor"], &none)?;
    c.check(
        "Отказ не превратился в установку",
        addon_state(&d.json, CONNECT) == "missing" && d.result()["ready"] == false,
        d.result()["addons"].clone(),
    )?;
    c.quit_via_gui(&none)?;
    fs::remove_dir_all(&c.base).map_err(|e| Fail::error("io", e.to_string()))?;

    c.step("отключённый аддон");
    fx.base_with_addons(c)?;
    let meta = c.base.join("addons21").join(CROWD).join("meta.json");
    let mut value: Value =
        serde_json::from_slice(&fs::read(&meta).unwrap_or_else(|_| b"{}".to_vec()))
            .unwrap_or(json!({}));
    // Тот же признак записывает переключатель Tools → Add-ons в Anki.
    value["disabled"] = json!(true);
    fs::write(&meta, serde_json::to_vec(&value).expect("json"))
        .map_err(|e| Fail::error("io", e.to_string()))?;
    let d = c.anki_repo(&["doctor"], &none)?;
    c.check(
        "doctor различает disabled",
        addon_state(&d.json, CROWD) == "disabled",
        d.result()["addons"].clone(),
    )?;
    let s = c.anki_repo(&["setup", "--apply"], &none)?;
    c.check(
        "setup не включает и не перезаписывает отключённый аддон",
        s.exit != 0 && s.error_reason() == "setup_blocked",
        s.json.clone(),
    )?;
    let p = c.anki_repo(&["patch", "install", "--apply"], &none)?;
    c.check(
        "patch install отказывает при отключённом CrowdAnki",
        p.exit != 0 && p.error_reason() == "incompatible_addon",
        p.json.clone(),
    )?;
    fs::remove_dir_all(&c.base).map_err(|e| Fail::error("io", e.to_string()))?;

    c.step("конфликт endpoint");
    fx.base_patched(c)?;
    let squatter = std::net::TcpListener::bind("127.0.0.1:8765")
        .map_err(|e| Fail::error("port_busy", e.to_string()))?;
    squatter
        .set_nonblocking(true)
        .map_err(|e| Fail::error("io", e.to_string()))?;
    let port_policy = Policy::none().click("ankiconnect_port_in_use", "OK", 1);
    launch_owned_with(c, &port_policy)?;
    c.observe(Duration::from_secs(8), &port_policy)?;
    let d = c.anki_repo(&["doctor"], &port_policy)?;
    c.check(
        "Чужой слушатель на порту не получает секретов: wrong_service",
        d.result()["live"]["state"] == "wrong_service" && d.result()["ready"] == false,
        d.result()["live"].clone(),
    )?;
    let received = squatter.accept().is_ok();
    c.check(
        "Чужой слушатель не получил ни одного запроса",
        !received,
        json!(received),
    )?;
    drop(squatter);
    c.quit_via_gui(&port_policy)?;

    c.step("конфликт профиля");
    launch_owned(c)?;
    wait_live(c, "patch_loaded")?;
    let base = c.base.to_string_lossy().into_owned();
    let w = c.anki_repo_raw(
        &[
            "--json",
            "anki",
            "--data-dir",
            &base,
            "--profile",
            "Other profile",
            "doctor",
        ],
        &none,
    )?;
    c.check(
        "Другой профиль распознан как wrong_profile",
        w.result()["live"]["state"] == "wrong_profile",
        w.result()["live"].clone(),
    )?;
    let e = c.anki_repo_raw(
        &[
            "--json",
            "anki",
            "--data-dir",
            &base,
            "--profile",
            "Other profile",
            "export",
            "--deck",
            DECK,
            "--apply",
        ],
        &none,
    )?;
    c.check(
        "Операция для чужого профиля отклонена",
        e.exit != 0,
        e.json.clone(),
    )?;
    stop_owned(c)?;

    c.step("сломанный аддон");
    let init = c.base.join("addons21").join(CONNECT).join("__init__.py");
    let original = fs::read(&init).map_err(|e| Fail::error("io", e.to_string()))?;
    // Исключение до любого побочного эффекта модуля: аддон не загружается совсем.
    fs::write(
        &init,
        [
            b"raise RuntimeError('e2e broken add-on')\n".as_slice(),
            original.as_slice(),
        ]
        .concat(),
    )
    .map_err(|e| Fail::error("io", e.to_string()))?;
    let broken = Policy::none().click("anki_addon_startup_failed", "Close", 1);
    launch_owned_with(c, &broken)?;
    let step = c.ui.step.clone();
    c.wait(
        "предупреждение Anki об ошибке аддона",
        Duration::from_secs(60),
        &broken,
        |c| Ok(c.ui.seen_in_step("anki_addon_startup_failed", &step) == 1),
    )?;
    let d = c.anki_repo(&["doctor"], &broken)?;
    c.check(
        "Сломанный AnkiConnect: диск unsupported, API недоступен, не ready",
        addon_state(&d.json, CONNECT) == "unsupported"
            && d.result()["live"]["state"] == "http_unreachable"
            && d.result()["ready"] == false,
        d.result().clone(),
    )?;
    c.quit_via_gui(&broken)?;
    fs::write(&init, &original).map_err(|e| Fail::error("io", e.to_string()))?;
    Ok(())
}

fn launch_owned_with(c: &mut Ctx, policy: &Policy) -> Outcome {
    let l = c.anki_repo(&["launch", "--apply"], policy)?;
    c.check(
        "launch --apply → starting",
        l.state() == "starting",
        l.json.clone(),
    )?;
    c.wait(
        "главное окно",
        Duration::from_secs(120),
        policy,
        |c| Ok(c.ui.main_window().is_some()),
    )
}

pub fn by_name(name: &str) -> Option<fn(&mut Ctx, &mut Fixtures) -> Outcome> {
    Some(match name {
        "smoke" => smoke,
        "addons" => addons,
        "patch" => patch,
        "crowdanki" => crowdanki,
        "process" => process,
        "errors" => errors,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_import_changes_a_root_or_nested_note_only_once() {
        let note = json!({"guid":"fixture-note","fields":["front","back"]});
        for mut deck in [
            json!({"notes":[note],"children":[]}),
            json!({"notes":[],"children":[{"notes":[],"children":[{"notes":[note]}]}]}),
        ] {
            assert!(change_cancelled_import_field(&mut deck));
            assert_eq!(
                export_notes(&deck),
                vec![
                    json!({"guid":"fixture-note","fields":["front","changed by cancelled import"]})
                ]
            );
            assert!(!change_cancelled_import_field(&mut deck));
        }
    }

    #[test]
    fn cancelled_import_rejects_an_empty_or_malformed_fixture() {
        for mut deck in [
            json!({"notes":[],"children":[{"notes":[]}]}),
            json!({"notes":[{"fields":["front"]}]}),
            json!({"notes":[{"fields":null}]}),
        ] {
            let before = deck.clone();
            assert!(!change_cancelled_import_field(&mut deck));
            assert_eq!(deck, before);
        }
    }
}
