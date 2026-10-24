use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    index::{ExportIndex, resolve_named_fields},
    loader,
    ops::{source, validate},
};

use super::{DeckIdentity, ExchangeError, ProfileId, Result, digest, export_name, valid_hash};

/// Адрес поля в семантическом отчёте, разрешённый общим ExportIndex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldAddress {
    pub model_uuid: String,
    pub name: String,
    pub ord: i64,
}

/// Семантический индекс: хеши адресуемых единиц, без текстов полей и копий media.
/// Закрытые поля не позволяют объявить произвольный SHA проверенным снимком.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    identity: DeckIdentity,
    name: String,
    units: BTreeMap<String, String>,
    layouts: BTreeMap<String, String>,
    fields: BTreeMap<String, FieldAddress>,
    opaque: BTreeSet<String>,
    unproven: BTreeSet<String>,
    revision: String,
}

impl Snapshot {
    /// Данные будущего адаптера. `media` — полный набор физических файлов, включая
    /// необъявленные. Отсутствующее объявленное media остаётся недоказанным.
    /// Метод не открывает ни одного пути, в том числе `decks/input`.
    pub fn from_bytes(
        profile: ProfileId,
        deck_json: &[u8],
        media: &BTreeMap<String, Vec<u8>>,
    ) -> Result<Self> {
        let hashes = media
            .iter()
            .map(|(name, bytes)| (name.clone(), asset_store::hashing::sha256_hex(bytes)))
            .collect();
        Self::from_hashes(profile, deck_json, hashes)
    }

    /// Обычное чтение рабочей копии; запрет input действует до открытия файлов.
    pub fn read_working(profile: ProfileId, directory: &Path) -> Result<Self> {
        crate::layout::guard_local_path(directory, "рабочий экспорт обмена")?;
        let directory = super::filesystem::Directory::open(directory)?;
        Self::read_directory(profile, &directory)
    }

    pub(super) fn read_directory(
        profile: ProfileId,
        directory: &super::filesystem::Directory,
    ) -> Result<Self> {
        // Та же рекомендательная блокировка inode каталога, что у write::ExportLock.
        let _lock = directory.lock()?;
        let bytes = directory.read("deck.json")?;
        let media = match directory.child("media")? {
            None => BTreeMap::new(),
            Some(media) => media.hash_files()?,
        };
        Self::from_hashes(profile, &bytes, media)
    }

    fn from_hashes(
        profile: ProfileId,
        bytes: &[u8],
        physical: BTreeMap<String, String>,
    ) -> Result<Self> {
        let path = Path::new("deck.json");
        let value = loader::parse_deck_json_bytes(bytes, path)?;
        let root = loader::typed_root(value.clone(), path)?;
        let validation = validate::validate_structure(&root);
        if !validation.valid
            || validation.issues.iter().any(|issue| {
                matches!(
                    issue.code,
                    validate::warning_codes::CONFLICTING_NOTE_MODEL_DEFINITION
                        | validate::warning_codes::CONFLICTING_DECK_CONFIG_DEFINITION
                        | validate::warning_codes::TEMPLATE_ORD_INVALID
                        | validate::warning_codes::TEMPLATE_ORD_DUPLICATE
                )
            })
        {
            return Err(ExchangeError::InvalidSnapshot(format!(
                "структурная проверка: {:?}",
                validation
                    .issues
                    .iter()
                    .filter(|i| i.severity != validate::Severity::Info)
                    .map(|i| i.code)
                    .collect::<Vec<_>>()
            )));
        }
        let identity = DeckIdentity::new(profile, string(&value, "crowdanki_uuid")?)?;
        let name = string(&value, "name")?.to_owned();
        export_name(&name)?;
        let mut snapshot = Self {
            identity,
            name,
            units: BTreeMap::new(),
            layouts: BTreeMap::new(),
            fields: BTreeMap::new(),
            opaque: BTreeSet::new(),
            unproven: BTreeSet::new(),
            revision: String::new(),
        };
        let paths = source::deck_child_paths(&value)?;
        let index = ExportIndex::build(&root);
        let mut deck_ids = BTreeSet::new();
        let mut deck_names = BTreeSet::new();
        let mut declared = BTreeSet::new();
        let mut references = BTreeSet::new();
        for children in &paths {
            let node = source::node_ref(&value, children).expect("проверенное дерево");
            let uuid = string(node, "crowdanki_uuid")?;
            if !deck_ids.insert(uuid) {
                return Err(ExchangeError::InvalidSnapshot(
                    "повторный UUID колоды".into(),
                ));
            }
            let normalized_name = export_name(string(node, "name")?)?;
            if !deck_names.insert(super::identity::path_key(&normalized_name)) {
                return Err(ExchangeError::InvalidSnapshot(
                    "неоднозначные имена узлов колод после нормализации".into(),
                ));
            }
            if let Some(desc) = node.get("desc").and_then(Value::as_str) {
                snapshot.html_references(
                    desc,
                    &format!("/decks/{}/desc", escape(uuid)),
                    &mut references,
                );
            }
            let prefix = format!("/decks/{}", escape(uuid));
            let parent = if children.is_empty() {
                Value::Null
            } else {
                let parent =
                    source::node_ref(&value, &children[..children.len() - 1]).expect("родитель");
                json!(string(parent, "crowdanki_uuid")?)
            };
            // Вычисленные связи живут вне пространства исходных свойств: неизвестный
            // ключ `parent`/`deck` не должен затереть настоящую принадлежность.
            snapshot.put(format!("/hierarchy/{}/parent", escape(uuid)), &parent);
            snapshot.properties(
                node,
                &prefix,
                &[
                    "children",
                    "notes",
                    "note_models",
                    "deck_configurations",
                    "media_files",
                ],
                &[
                    "__type__",
                    "crowdanki_uuid",
                    "name",
                    "deck_config_uuid",
                    "desc",
                    "dyn",
                    "extendNew",
                    "extendRev",
                ],
            );
            let names = strings(node, "media_files")?;
            for name in &names {
                super::filesystem::plain_filename(name)?;
            }
            declared.extend(names.iter().cloned());
            snapshot.put(format!("{prefix}/media_files"), &names);
            for (collection, section) in [
                ("note_models", "models"),
                ("deck_configurations", "configs"),
            ] {
                let mut declarations = BTreeSet::new();
                for definition in array(node, collection)? {
                    let id = string(definition, "crowdanki_uuid")?;
                    declarations.insert(id);
                    let mut normalized = definition.clone();
                    if collection == "note_models" {
                        sort_by_ord(&mut normalized, "flds")?;
                        sort_by_ord(&mut normalized, "tmpls")?;
                        snapshot.layouts.insert(id.into(), digest(&json!({
                            "fields": normalized["flds"], "type": normalized.get("type"),
                            "template_ords": array(&normalized, "tmpls")?.iter().map(|t| &t["ord"]).collect::<Vec<_>>()
                        })));
                    }
                    let key = format!("/{section}/{}", escape(id));
                    // Неизвестное свойство сохраняется в общем хеше и отдельно
                    // адресуется в opaque, чтобы его изменение не исчезло.
                    let known: &[&str] = if section == "models" {
                        &[
                            "__type__",
                            "crowdanki_uuid",
                            "name",
                            "type",
                            "css",
                            "flds",
                            "tmpls",
                            "req",
                            "latexPre",
                            "latexPost",
                            "latexsvg",
                            "sortf",
                            "tags",
                            "vers",
                        ]
                    } else {
                        &[
                            "__type__",
                            "crowdanki_uuid",
                            "name",
                            "autoplay",
                            "dyn",
                            "lapse",
                            "maxTaken",
                            "new",
                            "replayq",
                            "rev",
                            "timer",
                            "usn",
                            "mod",
                            "newMix",
                            "newPerDay",
                            "revPerDay",
                            "desiredRetention",
                            "fsrsWeights",
                        ]
                    };
                    for (property, value) in normalized
                        .as_object()
                        .expect("определение модели/конфигурации")
                    {
                        if !known.contains(&property.as_str()) {
                            snapshot.opaque_property(&format!("{key}/{}", escape(property)), value);
                        } else if !matches!(property.as_str(), "flds" | "tmpls")
                            || section != "models"
                        {
                            // Для вложенных объектов, например new/rev/lapse, схема
                            // здесь не интерпретируется: изменение недоказанно целиком.
                            snapshot.opaque_objects(&format!("{key}/{}", escape(property)), value);
                        }
                    }
                    if section == "models" {
                        let model = index.model_by_uuid(id).expect("валидатор разрешил модель");
                        // Неизвестные свойства берём из общего типизированного ядра,
                        // адресуем по ord, а не по порядку определений в исходном массиве.
                        for field in &model.flds {
                            for (property, value) in &field.extra {
                                snapshot.opaque_property(
                                    &format!("{key}/flds/{}/{}", field.ord, escape(property)),
                                    value,
                                );
                            }
                        }
                        for template in &model.tmpls {
                            for (property, value) in &template.extra {
                                if property != "__type__" {
                                    snapshot.opaque_property(
                                        &format!(
                                            "{key}/tmpls/{}/{}",
                                            template.ord,
                                            escape(property)
                                        ),
                                        value,
                                    );
                                }
                            }
                        }
                        // Составное значение даже известного свойства (например,
                        // нетекстовое description) не доказывает семантику его ключей.
                        for collection in ["flds", "tmpls"] {
                            for component in array(&normalized, collection)? {
                                for (property, value) in component.as_object().expect("поле/шаблон")
                                {
                                    snapshot.opaque_objects(
                                        &format!(
                                            "{key}/{collection}/{}/{}",
                                            component["ord"],
                                            escape(property)
                                        ),
                                        value,
                                    );
                                }
                            }
                        }
                    }
                    let hash = digest(&normalized);
                    if snapshot
                        .units
                        .get(&key)
                        .is_some_and(|previous| previous != &hash)
                    {
                        return Err(ExchangeError::InvalidSnapshot(
                            "неоднозначное определение".into(),
                        ));
                    }
                    snapshot.units.insert(key, hash);
                }
                snapshot.put(format!("{prefix}/{collection}"), &declarations);
            }
        }
        let raw_notes = source::collect_value_notes(&value)?;
        source::ensure_note_correspondence(&index, &raw_notes)?;
        for (typed, raw) in index.notes.iter().zip(raw_notes) {
            let note = source::note_ref(&value, &raw.path).expect("проверенная заметка");
            let prefix = format!("/notes/{}", escape(string(note, "guid")?));
            let model_id = string(note, "note_model_uuid")?;
            let model = index
                .model_by_uuid(model_id)
                .expect("валидатор разрешил модель");
            snapshot.put(
                format!("/membership/{}/deck", escape(string(note, "guid")?)),
                &index.nodes[typed.node].node.crowdanki_uuid,
            );
            snapshot.properties(
                note,
                &prefix,
                &["fields", "tags"],
                &["__type__", "guid", "note_model_uuid"],
            );
            snapshot.put(
                format!("{prefix}/tags"),
                &strings(note, "tags")?
                    .into_iter()
                    .map(|t| t.to_lowercase())
                    .collect::<BTreeSet<_>>(),
            );
            for field in resolve_named_fields(typed.note, model) {
                if let Some(text) = field.value.and_then(|v| v.as_text()) {
                    snapshot.html_references(
                        text,
                        &format!("{prefix}/fields/{}", field.ord),
                        &mut references,
                    );
                }
                let key = format!("{prefix}/fields/{}", field.ord);
                snapshot.fields.insert(
                    key.clone(),
                    FieldAddress {
                        model_uuid: model_id.into(),
                        name: field.name.into(),
                        ord: field.ord.value().expect("валидный ord"),
                    },
                );
                snapshot.put(key, &json!({"model": model_id, "name": field.name, "ord": field.ord.value(),
                    "value": field.value.and_then(|v| v.as_text()).expect("валидатор проверил поля")}));
            }
        }
        for model in &index.models {
            if let Some(css) = &model.css {
                references.extend(crate::report::css::media_addresses(css));
            }
            for template in &model.tmpls {
                let id = model
                    .crowdanki_uuid
                    .as_deref()
                    .expect("идентичность модели");
                snapshot.html_references(
                    &template.qfmt,
                    &format!("/models/{}/tmpls/{}/qfmt", escape(id), template.ord),
                    &mut references,
                );
                snapshot.html_references(
                    &template.afmt,
                    &format!("/models/{}/tmpls/{}/afmt", escape(id), template.ord),
                    &mut references,
                );
            }
        }
        for reference in references {
            match crate::media::consumer_media_name(&reference) {
                Some(name) if reference == name && !name.contains("{{") && !name.contains("}}") => {
                    if !declared.contains(&name) || !physical.contains_key(&name) {
                        snapshot.unproven.insert(format!("unresolved_media:{name}"));
                    }
                }
                _ => {
                    snapshot
                        .unproven
                        .insert(format!("unproven_media_reference:{reference}"));
                }
            }
        }
        for name in declared.difference(&physical.keys().cloned().collect()) {
            snapshot.unproven.insert(format!("missing_media:{name}"));
        }
        for (name, hash) in physical {
            super::filesystem::plain_filename(&name)?;
            snapshot
                .units
                .insert(format!("/media/{}", escape(&name)), hash);
        }
        // Статический анализ не доказывает вычисляемые ссылки и семантику
        // незнакомых шаблонных конструкций. Они видны вызывающему коду.
        for issue in &validation.issues {
            if issue.code == validate::warning_codes::TEMPLATE_CONSTRUCT_UNCHECKED {
                snapshot
                    .unproven
                    .insert(format!("{}:{}", issue.code, digest(&issue.details)));
            }
        }
        snapshot.revision = snapshot.content_hash();
        snapshot
            .verify()
            .map_err(|error| ExchangeError::InvalidSnapshot(error.to_string()))?;
        Ok(snapshot)
    }

    fn properties(&mut self, value: &Value, prefix: &str, excluded: &[&str], understood: &[&str]) {
        for (key, value) in value
            .as_object()
            .expect("типизированное ядро проверило объект")
        {
            if excluded.contains(&key.as_str()) {
                continue;
            }
            let path = format!("{prefix}/{}", escape(key));
            self.put(path.clone(), value);
            if !understood.contains(&key.as_str()) {
                self.opaque.insert(path);
            }
        }
    }

    fn opaque_property(&mut self, path: &str, value: &Value) {
        let path = format!("/opaque{path}");
        self.put(path.clone(), value);
        self.opaque.insert(path);
    }

    fn opaque_objects(&mut self, path: &str, value: &Value) {
        match value {
            Value::Object(_) => self.opaque_property(path, value),
            Value::Array(values) => {
                for (position, value) in values.iter().enumerate() {
                    self.opaque_objects(&format!("{path}/{position}"), value);
                }
            }
            _ => {}
        }
    }

    fn html_references(&mut self, text: &str, address: &str, references: &mut BTreeSet<String>) {
        references.extend(crate::media::consumer_media_references(text));
        for tag in crate::htmlscan::scan_tags(text) {
            match tag {
                crate::htmlscan::Tag::RawText { name, .. }
                    if name.eq_ignore_ascii_case("script") =>
                {
                    self.unproven.insert(format!("dynamic_html:{address}"));
                }
                crate::htmlscan::Tag::Element(element)
                    if ["script", "iframe", "object", "embed"]
                        .iter()
                        .any(|name| element.name.eq_ignore_ascii_case(name))
                        || element
                            .attributes
                            .iter()
                            .any(|a| a.name.to_ascii_lowercase().starts_with("on")) =>
                {
                    self.unproven.insert(format!("dynamic_html:{address}"));
                }
                crate::htmlscan::Tag::Unterminated { .. } => {
                    self.unproven.insert(format!("incomplete_html:{address}"));
                }
                crate::htmlscan::Tag::RawText {
                    close_end: None, ..
                } => {
                    self.unproven.insert(format!("incomplete_html:{address}"));
                }
                _ => {}
            }
        }
    }

    fn put(&mut self, path: String, value: &impl Serialize) {
        self.units.insert(path, digest(value));
    }

    pub fn identity(&self) -> &DeckIdentity {
        &self.identity
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn revision(&self) -> &str {
        &self.revision
    }
    pub fn unproven(&self) -> &BTreeSet<String> {
        &self.unproven
    }

    pub fn field_address(&self, path: &str) -> Option<&FieldAddress> {
        self.fields.get(path)
    }
    pub(super) fn units(&self) -> &BTreeMap<String, String> {
        &self.units
    }
    pub(super) fn layouts(&self) -> &BTreeMap<String, String> {
        &self.layouts
    }
    pub(super) fn opaque(&self) -> &BTreeSet<String> {
        &self.opaque
    }

    fn content_hash(&self) -> String {
        digest(&(
            &self.identity,
            &self.name,
            &self.units,
            &self.layouts,
            &self.fields,
            &self.opaque,
            &self.unproven,
        ))
    }

    pub(super) fn verify(&self) -> Result<()> {
        self.identity.validate()?;
        export_name(&self.name)?;
        if self.revision != self.content_hash()
            || self.units.is_empty()
            || self
                .units
                .values()
                .chain(self.layouts.values())
                .any(|v| !valid_hash(v))
            || self.opaque.iter().any(|key| !self.units.contains_key(key))
            || self.fields.iter().any(|(key, field)| {
                !self.units.contains_key(key)
                    || field.ord < 0
                    || field.name.is_empty()
                    || !self.layouts.contains_key(&field.model_uuid)
            })
        {
            return Err(ExchangeError::CorruptState(
                "целостность семантического снимка".into(),
            ));
        }
        Ok(())
    }
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    let result = value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ExchangeError::InvalidSnapshot(format!("нет строкового {key}")))?;
    if result.chars().any(char::is_control) {
        return Err(ExchangeError::InvalidSnapshot(format!(
            "управляющий символ в {key}"
        )));
    }
    Ok(result)
}

fn array<'a>(value: &'a Value, key: &str) -> Result<&'a [Value]> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(values)) => Ok(values),
        _ => Err(ExchangeError::InvalidSnapshot(format!("{key} не массив"))),
    }
}

fn strings(value: &Value, key: &str) -> Result<BTreeSet<String>> {
    array(value, key)?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| ExchangeError::InvalidSnapshot(format!("{key} содержит не строку")))
        })
        .collect()
}

fn sort_by_ord(value: &mut Value, key: &str) -> Result<()> {
    match value.get_mut(key) {
        Some(Value::Array(values)) => {
            if values
                .iter()
                .any(|v| v.get("ord").and_then(Value::as_u64).is_none())
            {
                return Err(ExchangeError::InvalidSnapshot(format!(
                    "некорректный ord в {key}"
                )));
            }
            values.sort_by_key(|v| v["ord"].as_u64());
            Ok(())
        }
        None | Some(Value::Null) => Ok(()),
        _ => Err(ExchangeError::InvalidSnapshot(format!("{key} не массив"))),
    }
}

pub(super) fn escape(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}
