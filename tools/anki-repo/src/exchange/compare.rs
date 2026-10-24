use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::{ExchangeError, FieldAddress, Result, Snapshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Unchanged,
    InputOnly,
    WorkingOnly,
    Converged,
    Diverged,
    Conflict,
}

/// Адрес и хеши значений. Отсутствие — добавление/удаление, а не пустое поле.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Difference {
    pub path: String,
    pub field: Option<FieldAddress>,
    pub base: Option<String>,
    pub input: Option<String>,
    pub working: Option<String>,
    pub conflict: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Comparison {
    pub disposition: Disposition,
    pub differences: Vec<Difference>,
    pub incompatible: Vec<String>,
    pub unproven: Vec<String>,
}

impl Comparison {
    pub fn proven_compatible(&self) -> bool {
        self.incompatible.is_empty() && self.unproven.is_empty()
    }
}

/// Детерминированное A/C/B сравнение без слияния и без изменения экспортов.
pub fn compare(base: &Snapshot, input: &Snapshot, working: &Snapshot) -> Result<Comparison> {
    base.verify()?;
    input.verify()?;
    working.verify()?;
    if base.identity() != input.identity() || base.identity() != working.identity() {
        return Err(ExchangeError::IdentityMismatch);
    }
    let keys: BTreeSet<_> = base
        .units()
        .keys()
        .chain(input.units().keys())
        .chain(working.units().keys())
        .collect();
    let mut differences = Vec::new();
    let mut input_changed = false;
    let mut working_changed = false;
    let mut unproven: BTreeSet<String> = base
        .unproven()
        .iter()
        .chain(input.unproven())
        .chain(working.unproven())
        .cloned()
        .collect();
    for key in keys {
        let a = base.units().get(key);
        let c = input.units().get(key);
        let b = working.units().get(key);
        if a == c && a == b {
            continue;
        }
        input_changed |= a != c;
        working_changed |= a != b;
        if base.opaque().contains(key)
            || input.opaque().contains(key)
            || working.opaque().contains(key)
        {
            unproven.insert(format!("opaque_change:{key}"));
        }
        differences.push(Difference {
            path: key.clone(),
            field: working
                .field_address(key)
                .or_else(|| input.field_address(key))
                .or_else(|| base.field_address(key))
                .cloned(),
            base: a.cloned(),
            input: c.cloned(),
            working: b.cloned(),
            conflict: a != c && a != b && c != b,
        });
    }
    let mut incompatible = Vec::new();
    let model_ids: BTreeSet<_> = base
        .layouts()
        .keys()
        .chain(input.layouts().keys())
        .chain(working.layouts().keys())
        .collect();
    for id in model_ids {
        let a = base.layouts().get(id);
        // Добавление новой модели допустимо; смена/удаление прежней схемы —
        // отдельная миграция, которую этот контракт не доказывает.
        if a.is_some() && (a != input.layouts().get(id) || a != working.layouts().get(id)) {
            incompatible.push(format!("model_layout:{id}"));
        }
    }
    for difference in &differences {
        if difference.path.starts_with("/notes/")
            && difference.path.ends_with("/note_model_uuid")
            && difference.base.is_some()
            && ((difference.input.is_some() && difference.input != difference.base)
                || (difference.working.is_some() && difference.working != difference.base))
        {
            incompatible.push(format!("note_model:{}", difference.path));
        }
    }
    // Удаление сущности против изменения любого её свойства — конфликт,
    // даже если изменённое свойство было добавлено только на другой стороне.
    let mut removed = BTreeMap::new();
    let mut removed_decks = BTreeMap::new();
    for difference in &differences {
        if difference.base.is_some()
            && (difference.input.is_none() || difference.working.is_none())
            && let Some(prefix) = entity(&difference.path)
            && matches!(
                &difference.path[prefix.len()..],
                "/guid" | "/crowdanki_uuid"
            )
        {
            removed.insert(
                prefix.to_string(),
                (difference.input.is_none(), difference.working.is_none()),
            );
            if difference.path.starts_with("/decks/") {
                // Связь хранит SHA строкового UUID, как и свойство crowdanki_uuid.
                // Исходные UUID восстанавливать из адреса или хранить второй раз не нужно.
                removed_decks.insert(
                    difference.base.clone().expect("исходный UUID"),
                    (difference.input.is_none(), difference.working.is_none()),
                );
            }
        }
    }
    for difference in &mut differences {
        if let Some((input_removed, working_removed)) =
            entity(&difference.path).and_then(|prefix| removed.get(prefix))
            && difference.input != difference.working
            && ((*input_removed && difference.base != difference.working)
                || (*working_removed && difference.base != difference.input))
        {
            difference.conflict = true;
        }
        // Новая/изменённая связь с удалённым на другой стороне родителем —
        // конфликт зависимости, даже если самой дочерней сущности в A ещё не было.
        if difference.path.starts_with("/membership/") || difference.path.starts_with("/hierarchy/")
        {
            let input_parent_removed = difference
                .working
                .as_ref()
                .and_then(|parent| removed_decks.get(parent))
                .is_some_and(|(input_removed, _)| *input_removed);
            let working_parent_removed = difference
                .input
                .as_ref()
                .and_then(|parent| removed_decks.get(parent))
                .is_some_and(|(_, working_removed)| *working_removed);
            if (input_parent_removed && difference.base != difference.working)
                || (working_parent_removed && difference.base != difference.input)
            {
                difference.conflict = true;
            }
        }
    }
    let conflict = differences.iter().any(|d| d.conflict);
    let disposition = match (input_changed, working_changed) {
        (false, false) => Disposition::Unchanged,
        (true, false) => Disposition::InputOnly,
        (false, true) => Disposition::WorkingOnly,
        (true, true) if input.revision() == working.revision() => Disposition::Converged,
        (true, true) if conflict => Disposition::Conflict,
        (true, true) => Disposition::Diverged,
    };
    Ok(Comparison {
        disposition,
        differences,
        incompatible,
        unproven: unproven.into_iter().collect(),
    })
}

fn entity(path: &str) -> Option<&str> {
    if !path.starts_with("/notes/") && !path.starts_with("/decks/") {
        return None;
    }
    let end = path[1..].find('/').map(|i| i + 2)?;
    let tail = path[end..].find('/').map(|i| i + end).unwrap_or(path.len());
    Some(&path[..tail])
}
