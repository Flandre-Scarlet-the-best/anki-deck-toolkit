use super::{COMPATIBILITY, CONNECT, CROWD, Options, Result, error, external_error, io_error};
use super::{
    environment::{self, Target},
    patch,
};
use serde_json::{Value, json};
use std::{
    io::{Cursor, Read, Write},
    time::Duration,
};

pub(crate) fn run(t: &Target, _o: &Options, apply: bool) -> Result<Value> {
    // Общий владелец от повторной проверки состояния до передачи установщику.
    // backup не берёт вложенный lock; после spawn дальнейшую защиту даёт процесс Anki.
    let _lock = if apply && !t.running {
        Some(patch::lock_addons(t)?)
    } else {
        None
    };
    let mut missing = Vec::new();
    let mut blocked = Vec::new();
    for id in [CONNECT, CROWD] {
        let s = patch::addon_status(t, id)?;
        if s["state"] == "missing" {
            missing.push(id);
        } else if s["state"] != "installed_unverified" {
            blocked.push(s);
        }
    }
    let plan = json!({"state":"setup_plan","ready":false,"missing":missing,"blocked":blocked,
        "sources":missing.iter().map(|id|format!("https://ankiweb.net/shared/info/{id}")).collect::<Vec<_>>(),
        "method":"official_ankiweb_packages_via_native_anki_package_installer",
        "consequences":"Anki покажет штатное подтверждение установки .ankiaddon. После установки закройте Anki, установите patch и запустите повторно; doctor проверит загрузку. Существующие здоровые аддоны не переустанавливаются."});
    if !apply {
        return Ok(plan);
    }
    if !blocked.is_empty() {
        return Err(error(
            "setup_blocked",
            "Отключённый/неподдерживаемый аддон требует явного решения; setup его не перезаписывает",
        ));
    }
    if missing.is_empty() {
        return Ok(json!({"state":"no_installation_needed","ready":false,"next":"doctor"}));
    }
    if t.running {
        return Ok(
            json!({"state":"restart_required","ready":false,"reason":"native_installation_requires_stopped_anki","plan":plan}),
        );
    }
    let compatibility: Value = serde_json::from_str(COMPATIBILITY).map_err(io_error)?;
    if !patch::supports_anki(&t.version, &compatibility) {
        return Err(error(
            "unsupported_anki",
            "Для этой версии Anki нет проверенного адаптера; установка не выполняется",
        ));
    }
    patch::ensure_private(&t.state())?;
    let backup = patch::backup(t)?;
    let root = t.state().join("packages").join(patch::random_id()?);
    patch::ensure_private(&root)?;
    let mut packages = Vec::new();
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(90))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() > 4
                || attempt.url().scheme() != "https"
                || attempt.url().host_str() != Some("ankiweb.net")
            {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(io_error)?;
    for id in &missing {
        let url = format!(
            "https://ankiweb.net/shared/download/{id}?v=2.1&p={}",
            patch::point_version(&t.version).unwrap_or(0)
        );
        let response = client.get(&url).send().map_err(io_error)?;
        if !response.status().is_success()
            || response.url().host_str() != Some("ankiweb.net")
            || response.url().path() != format!("/svc/shared/download-addon/{id}")
        {
            return Err(external_error(
                "installer_download_failed",
                "AnkiWeb не предоставил официальный пакет для выбранной Anki",
            ));
        }
        let query: std::collections::BTreeMap<_, _> = response
            .url()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let meta_num = |key: &str| -> Result<i64> {
            query
                .get(key)
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| error("installer_metadata_invalid", key))
        };
        let mut bytes = Vec::new();
        response
            .take(20_000_001)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes.len() > 20_000_000 {
            return Err(error("installer_package_too_large", "Пакет больше предела"));
        }
        if json!(patch::hash(&bytes)) != compatibility["addons"][id]["archive_sha256"] {
            return Err(error(
                "unsupported_addon_package",
                "Официальный пакет изменился: его полная SHA-256 не входит в allowlist",
            ));
        }
        let mut archive = zip::ZipArchive::new(Cursor::new(&bytes)).map_err(io_error)?;
        let mut actual = std::collections::BTreeMap::new();
        let mut unpacked = Vec::new();
        let mut total = 0u64;
        for i in 0..archive.len() {
            let mut f = archive.by_index(i).map_err(io_error)?;
            let name = f.name().map_err(io_error)?.into_owned();
            if f.is_dir() {
                continue;
            }
            if f.enclosed_name().is_none()
                || name.contains(['\\', ':'])
                || f.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000)
                || f.size() > 30_000_000
            {
                return Err(error("unsafe_addon_package", "Небезопасный файл пакета"));
            }
            let mut content = Vec::new();
            f.by_ref()
                .take(30_000_001)
                .read_to_end(&mut content)
                .map_err(io_error)?;
            total += content.len() as u64;
            if total > 60_000_000 {
                return Err(error(
                    "unsafe_addon_package",
                    "Превышен распакованный размер",
                ));
            }
            if name.ends_with(".py") {
                actual.insert(name.clone(), patch::hash(&content));
            }
            unpacked.push((name, content));
        }
        if serde_json::to_value(actual).map_err(io_error)? != compatibility["addons"][id]["files"] {
            return Err(error(
                "unsupported_addon_package",
                "AnkiWeb обновил исходники: требуется новая проверка совместимости",
            ));
        }
        let original = unpacked
            .iter()
            .find(|(n, _)| n == "manifest.json")
            .ok_or_else(|| error("invalid_manifest", "Нет manifest.json"))?;
        let mut manifest: Value = serde_json::from_slice(&original.1).map_err(io_error)?;
        manifest["package"] = json!(id);
        manifest["mod"] = json!(meta_num("t")?);
        manifest["min_point_version"] = json!(meta_num("minpt")?);
        manifest["max_point_version"] = json!(meta_num("maxpt")?);
        manifest["branch_index"] = json!(meta_num("bidx")?);
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let manifest_bytes = serde_json::to_vec(&manifest).map_err(io_error)?;
        for (name, content) in unpacked {
            writer
                .start_file(&name, zip::write::SimpleFileOptions::default())
                .map_err(io_error)?;
            writer
                .write_all(if name == "manifest.json" {
                    &manifest_bytes
                } else {
                    &content
                })
                .map_err(io_error)?;
        }
        let normalized = writer.finish().map_err(io_error)?.into_inner();
        let package = root.join(format!("{id}.ankiaddon"));
        patch::write_atomic(&package, &normalized)?;
        patch::write_atomic(&root.join(format!("{id}.evidence.json")),&serde_json::to_vec_pretty(&json!({"source":url,"original_sha256":patch::hash(&bytes),"native_package_sha256":patch::hash(&normalized),"manifest":manifest})).map_err(io_error)?)?;
        packages.push(package);
    }
    // Anki принимает один package на запуск. Остальные открываются отдельно после
    // завершения первого диалога: пути возвращаются для управляемого продолжения.
    environment::require_stopped(t)?;
    environment::spawn(t, &packages[..1], None)?;
    Ok(
        json!({"state":"awaiting_native_installer","ready":false,"automatic_installation":false,"packages":packages,"backup":backup,
        "next":"Подтвердите штатный диалог Anki. Для следующего пакета откройте указанный .ankiaddon через Anki; затем перезапустите и выполните doctor. Установка ещё НЕ подтверждена."}),
    )
}
