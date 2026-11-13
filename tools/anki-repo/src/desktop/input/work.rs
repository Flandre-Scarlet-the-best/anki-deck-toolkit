//! Общие примитивы владельца подготовки/импорта output: журналы с контрольной
//! суммой, потоковое копирование полного экспорта и блокировка рабочей копии.
//!
//! Рабочая область `decks/.roundtrip/<ключ идентичности>/` приватна (0700) и
//! содержит только известные имена. Неизвестные объекты не удаляются.
use std::collections::BTreeMap;

use serde::{Serialize, de::DeserializeOwned};

use crate::exchange::{ExchangeError, ProfileId, Result, Snapshot, filesystem::Directory};

pub(super) const DECK: &str = "deck.json";
pub(super) const CONFIG: &str = "import_config.yaml";
pub(super) const MEDIA: &str = "media";
pub(super) const PREPARE: &str = "prepare.json";
pub(super) const PUBLISH: &str = "publish.json";
/// Маркер владения каталогом `spool/<job_id>` импорта; см. `publication`.
pub(super) const SPOOL: &str = "publish-spool.json";
pub(super) const STAGE: &str = "stage";
pub(super) const PREVIOUS: &str = "previous";

/// Полный физический инвентарь экспорта: относительное имя → SHA-256.
pub(super) type Inventory = BTreeMap<String, String>;

pub(super) fn corrupt(message: impl std::fmt::Display) -> ExchangeError {
    ExchangeError::CorruptState(message.to_string())
}

pub(super) fn blocked(reason: &str) -> ExchangeError {
    ExchangeError::Blocked(reason.into())
}

#[derive(Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope<T> {
    checksum: String,
    record: T,
}

fn checksum(value: &impl Serialize) -> Result<String> {
    let value = serde_json::to_value(value).map_err(corrupt)?;
    Ok(asset_store::hashing::sha256_hex(
        serde_json::to_vec(&value).map_err(corrupt)?,
    ))
}

pub(super) fn save<T: Serialize>(dir: &Directory, name: &str, record: &T) -> Result<()> {
    let envelope = Envelope {
        checksum: checksum(record)?,
        record,
    };
    dir.publish(name, &serde_json::to_vec(&envelope).map_err(corrupt)?)
}

pub(super) fn load<T: Serialize + DeserializeOwned>(
    dir: &Directory,
    name: &str,
) -> Result<Option<T>> {
    if dir.entry_is_dir(name)?.is_none() {
        return Ok(None);
    }
    let envelope: Envelope<T> = serde_json::from_slice(&dir.read(name)?).map_err(corrupt)?;
    if envelope.checksum != checksum(&envelope.record)? {
        return Err(corrupt(format!("контрольная сумма {name}")));
    }
    Ok(Some(envelope.record))
}

/// Каталог потока в рабочей области; посторонние имена — отказ без удаления.
pub(super) fn stream_dir(area: &Directory, key: &str) -> Result<Directory> {
    let (dir, _) = area.ensure_child(key)?;
    dir.require_private()?;
    if !dir.is_same_child(area, key)? {
        return Err(ExchangeError::UnsafePath("каталог потока подменён".into()));
    }
    for name in dir.names()? {
        let known = matches!(name.as_str(), PREPARE | PUBLISH | SPOOL | STAGE | PREVIOUS)
            || is_journal_temporary(&name);
        if !known {
            return Err(blocked("unknown_work_entries"));
        }
    }
    // Незавершённый temp атомарной записи журнала не является состоянием.
    for name in dir.names()? {
        if is_journal_temporary(&name) {
            dir.remove_file(&name)?;
        }
    }
    Ok(dir)
}

fn is_journal_temporary(name: &str) -> bool {
    super::publish::is_temporary(name, &[PREPARE, PUBLISH, SPOOL])
}

/// Потоковое чтение полного экспорта без advisory-блокировки: вызывающий
/// либо удерживает `ExportLock`, либо читает собственный приватный каталог.
/// Допустимы только `deck.json`, `import_config.yaml` и обычные файлы `media/`.
pub(super) fn read_inventory(dir: &Directory, profile: ProfileId) -> Result<(Snapshot, Inventory)> {
    let names = dir.names()?;
    if names
        .iter()
        .any(|n| !matches!(n.as_str(), DECK | MEDIA | CONFIG))
    {
        return Err(blocked("unexpected_export_entries"));
    }
    if dir.entry_is_dir(DECK)? != Some(false) {
        return Err(blocked("deck_json_missing"));
    }
    let json = dir.read(DECK)?;
    let mut inventory = Inventory::new();
    inventory.insert(DECK.into(), asset_store::hashing::sha256_hex(&json));
    if names.iter().any(|n| n == CONFIG) {
        inventory.insert(CONFIG.into(), dir.stream_hash(CONFIG)?.0);
    }
    let mut media_hashes = BTreeMap::new();
    let mut total = 0_u64;
    if names.iter().any(|n| n == MEDIA) {
        let media = dir
            .child(MEDIA)?
            .ok_or_else(|| ExchangeError::UnsafePath("media не каталог".into()))?;
        let listed = media.names()?;
        let mut folded = std::collections::BTreeSet::new();
        use unicode_normalization::UnicodeNormalization;
        for name in &listed {
            if !folded.insert(name.nfc().flat_map(char::to_lowercase).collect::<String>()) {
                return Err(ExchangeError::UnsafePath("коллизия имён media".into()));
            }
            let (hash, size) = media.stream_hash(name)?;
            total += size;
            if total > 1024 * 1024 * 1024 {
                return Err(ExchangeError::UnsafePath("экспорт превышает 1 GiB".into()));
            }
            inventory.insert(format!("{MEDIA}/{name}"), hash.clone());
            media_hashes.insert(name.clone(), hash);
        }
        if listed != media.names()? || !media.is_same_child(dir, MEDIA)? {
            return Err(ExchangeError::UnsafePath(
                "media изменились при чтении".into(),
            ));
        }
    }
    if names != dir.names()?
        || inventory.get(DECK) != Some(&asset_store::hashing::sha256_hex(dir.read(DECK)?))
    {
        return Err(ExchangeError::UnsafePath(
            "экспорт изменился при чтении".into(),
        ));
    }
    let snapshot = Snapshot::from_media_hashes(profile, &json, media_hashes)?;
    Ok((snapshot, inventory))
}

/// Копирует полный экспорт в новый пустой каталог, сверяя каждый файл с
/// ожидаемым инвентарём (если он задан). Возвращает инвентарь копии.
pub(super) fn copy_export(
    source: &Directory,
    target: &Directory,
    expected: Option<&Inventory>,
) -> Result<Inventory> {
    if !target.names()?.is_empty() {
        return Err(corrupt("каталог копии не пуст"));
    }
    let names = source.names()?;
    if names
        .iter()
        .any(|n| !matches!(n.as_str(), DECK | MEDIA | CONFIG))
    {
        return Err(blocked("unexpected_export_entries"));
    }
    let mut inventory = Inventory::new();
    for name in [DECK, CONFIG] {
        if names.iter().any(|n| n == name) {
            inventory.insert(name.into(), source.copy_into(name, target, name)?);
        }
    }
    if names.iter().any(|n| n == MEDIA) {
        let media = source
            .child(MEDIA)?
            .ok_or_else(|| ExchangeError::UnsafePath("media не каталог".into()))?;
        let (copy, _) = target.ensure_child(MEDIA)?;
        for name in media.names()? {
            inventory.insert(
                format!("{MEDIA}/{name}"),
                media.copy_into(&name, &copy, &name)?,
            );
        }
        copy.sync()?;
    }
    target.sync()?;
    if let Some(expected) = expected
        && &inventory != expected
    {
        return Err(corrupt(
            "скопированные байты не совпали с подтверждённым инвентарём",
        ));
    }
    Ok(inventory)
}

/// `ExportLock` той же рабочей копии, что открыта по dirfd без symlink.
/// Совпадение inode обязательно: путь не может увести блокировку в другой каталог.
pub(super) fn lock_export(
    output: &Directory,
    name: &str,
) -> std::result::Result<Option<(Directory, crate::write::ExportLock)>, ExchangeError> {
    let Some(dir) = output.child(name)? else {
        return Ok(None);
    };
    let lock = crate::write::ExportLock::acquire(&output.child_path(name)?)
        .map_err(ExchangeError::Export)?;
    if !dir.same_inode(&lock.directory)? || !dir.is_same_child(output, name)? {
        return Err(ExchangeError::UnsafePath(
            "рабочая копия подменена при блокировке".into(),
        ));
    }
    Ok(Some((dir, lock)))
}

/// Только имена различий A/C/B: значения полей и media в вывод не попадают.
pub(super) fn difference_summary(comparison: &crate::exchange::Comparison) -> serde_json::Value {
    let paths: Vec<_> = comparison
        .differences
        .iter()
        .take(50)
        .map(|d| serde_json::json!({"path":d.path,"conflict":d.conflict}))
        .collect();
    serde_json::json!({"disposition":comparison.disposition,
        "differences":comparison.differences.len(),"paths":paths,
        "incompatible":comparison.incompatible,"unproven":comparison.unproven})
}
