//! SQLite-индекс истории — производная проекция архива.
//!
//! Индекс ничего не хранит сверх архива: любую его строку можно получить из
//! исходного JSON, поэтому миграция схемы индекса — это пересборка из
//! архива, а повреждённый индекс заменяется новым без потери данных.
//! Версия схемы — `PRAGMA user_version`.
//!
//! Целостность SQLite (`quick_check`) и номер схемы не доказывают, что
//! проекция цела: при открытии сверяются и сама схема с ожидаемой, и
//! контрольная сумма строк каждой записи (`projection_sha256`). Строка с
//! несовпавшей суммой удаляется и строится заново из архива. Полная сверка
//! проекции с архивными JSON — [`Index::expected_digests`] в `verify`.

use benchmark_contract::result::{BenchmarkResult, Fact, OutcomeStatus, RunStatus};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Текущая версия схемы индекса.
pub const SCHEMA_VERSION: i32 = 2;

const SCHEMA: &str = r#"
CREATE TABLE results (
    id TEXT PRIMARY KEY,
    path TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    scenario_id TEXT NOT NULL,
    module TEXT NOT NULL,
    revision INTEGER NOT NULL,
    kind TEXT NOT NULL,
    profile_id TEXT NOT NULL,
    cache TEXT NOT NULL,
    network TEXT NOT NULL,
    outcome TEXT NOT NULL,
    outcome_reason TEXT,
    observation TEXT NOT NULL,
    runs INTEGER NOT NULL,
    successful_runs INTEGER NOT NULL,
    warmup_runs INTEGER NOT NULL,
    recorded_at TEXT NOT NULL,
    recorded_key TEXT NOT NULL,
    module_version TEXT NOT NULL,
    git_commit TEXT,
    dirty INTEGER,
    source_tree_sha256 TEXT,
    measurement TEXT NOT NULL,
    runner TEXT,
    projection_sha256 TEXT NOT NULL
) STRICT;
CREATE INDEX results_series ON results (scenario_id, profile_id, recorded_key);
CREATE TABLE inputs (
    result_id TEXT NOT NULL REFERENCES results(id) ON DELETE CASCADE,
    input_id TEXT NOT NULL,
    role TEXT NOT NULL,
    method TEXT NOT NULL,
    identity TEXT NOT NULL,
    PRIMARY KEY (result_id, input_id)
) STRICT;
CREATE TABLE input_sizes (
    result_id TEXT NOT NULL REFERENCES results(id) ON DELETE CASCADE,
    input_id TEXT NOT NULL,
    name TEXT NOT NULL,
    value REAL NOT NULL,
    unit TEXT NOT NULL,
    PRIMARY KEY (result_id, input_id, name)
) STRICT;
CREATE TABLE environment (
    result_id TEXT NOT NULL REFERENCES results(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    status TEXT NOT NULL,
    value TEXT,
    PRIMARY KEY (result_id, name)
) STRICT;
CREATE TABLE parameters (
    result_id TEXT NOT NULL REFERENCES results(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (result_id, name)
) STRICT;
CREATE TABLE metrics (
    result_id TEXT NOT NULL REFERENCES results(id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    metric TEXT NOT NULL,
    scope TEXT NOT NULL,
    source TEXT NOT NULL,
    unit TEXT NOT NULL,
    label TEXT,
    availability TEXT NOT NULL,
    reason TEXT,
    count INTEGER,
    mean REAL,
    stddev REAL,
    median REAL,
    min REAL,
    max REAL,
    PRIMARY KEY (result_id, key)
) STRICT;
"#;

const TABLES: &[&str] = &[
    "metrics",
    "parameters",
    "environment",
    "input_sizes",
    "inputs",
    "results",
];

/// Таблицы проекции с колонкой записи и порядком строк внутри записи.
const PROJECTION: &[(&str, &str, &str)] = &[
    ("results", "id", "id"),
    ("inputs", "result_id", "input_id"),
    ("input_sizes", "result_id", "input_id, name"),
    ("environment", "result_id", "name"),
    ("parameters", "result_id", "name"),
    ("metrics", "result_id", "key"),
];

const DIGEST_COLUMN: &str = "projection_sha256";

pub struct Index {
    conn: Connection,
}

/// Что произошло с индексом при открытии.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    Current,
    Created,
    /// Схема старее текущей: таблицы созданы заново, строки вернёт синхронизация.
    Migrated {
        from: i32,
    },
    /// Номер схемы текущий, но таблицы не совпадают с ожидаемыми: схема
    /// создана заново, строки вернёт синхронизация.
    Repaired {
        reason: String,
    },
}

fn text<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Время записи с дробной частью из девяти цифр: строки сравниваются
/// лексикографически в хронологическом порядке.
pub fn time_key(timestamp: &str) -> String {
    let body = timestamp.strip_suffix('Z').unwrap_or(timestamp);
    let (main, fraction) = body.split_once('.').unwrap_or((body, ""));
    format!("{main}.{fraction:0<9}Z")
}

impl Index {
    /// Открыть индекс и привести схему к текущей. Схема новее текущей —
    /// ошибка: её создал более новый инструмент, а пересборку выбирает
    /// пользователь (`reindex`).
    pub fn open(path: &Path) -> Result<(Index, Opened), rusqlite::Error> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if check != "ok" {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
                Some(format!("quick_check: {check}")),
            ));
        }
        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let mut index = Index { conn };
        let opened = match version {
            SCHEMA_VERSION => match index.schema_mismatch()? {
                None => Opened::Current,
                Some(reason) => {
                    index.create()?;
                    Opened::Repaired { reason }
                }
            },
            v if v > SCHEMA_VERSION => {
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_MISMATCH),
                    Some(format!(
                        "схема индекса {v} новее поддерживаемой {SCHEMA_VERSION}"
                    )),
                ));
            }
            0 if !index.has_tables()? => {
                index.create()?;
                Opened::Created
            }
            from => {
                index.create()?;
                Opened::Migrated { from }
            }
        };
        Ok((index, opened))
    }

    /// Индекс в памяти: эталон схемы и проекции для сверки.
    pub fn in_memory() -> rusqlite::Result<Index> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let mut index = Index { conn };
        index.create()?;
        Ok(index)
    }

    /// Объекты схемы (`type`, `name`, `sql`) без служебных объектов SQLite.
    fn schema(&self) -> rusqlite::Result<BTreeSet<(String, String, Option<String>)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT type, name, sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect()
    }

    /// Чем схема файла отличается от текущей; `None` — совпадает.
    fn schema_mismatch(&self) -> rusqlite::Result<Option<String>> {
        let expected = Index::in_memory()?.schema()?;
        let found = self.schema()?;
        if found == expected {
            return Ok(None);
        }
        let names = |set: &BTreeSet<(String, String, Option<String>)>| -> BTreeSet<String> {
            set.iter().map(|(_, name, _)| name.clone()).collect()
        };
        let (expected_names, found_names) = (names(&expected), names(&found));
        let missing: Vec<&String> = expected_names.difference(&found_names).collect();
        let extra: Vec<&String> = found_names.difference(&expected_names).collect();
        let mut parts = Vec::new();
        if !missing.is_empty() {
            parts.push(format!("нет {}", join(&missing)));
        }
        if !extra.is_empty() {
            parts.push(format!("лишние {}", join(&extra)));
        }
        if parts.is_empty() {
            parts.push("определения объектов изменены".into());
        }
        Ok(Some(format!(
            "схема индекса версии {SCHEMA_VERSION} не совпадает с ожидаемой: {}",
            parts.join("; ")
        )))
    }

    /// Контрольные суммы проекции, вычисленные по строкам таблиц; `only` —
    /// одна запись. Сумма охватывает все колонки всех таблиц проекции, кроме
    /// самой колонки суммы, поэтому любая правка строки её меняет.
    fn computed_digests(&self, only: Option<&str>) -> rusqlite::Result<BTreeMap<String, String>> {
        computed_digests(&self.conn, only)
    }

    fn stored_digests(&self) -> rusqlite::Result<BTreeMap<String, String>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT id, {DIGEST_COLUMN} FROM results"))?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect()
    }

    /// Записи, строки которых не совпадают с записанной контрольной суммой
    /// проекции, и записи с дочерними строками без строки `results`.
    pub fn inconsistent(&self) -> rusqlite::Result<Vec<String>> {
        let computed = self.computed_digests(None)?;
        let stored = self.stored_digests()?;
        Ok(computed
            .keys()
            .chain(stored.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|id| computed.get(*id) != stored.get(*id))
            .cloned()
            .collect())
    }

    /// Контрольные суммы проекции, вычисленные по текущим строкам индекса.
    pub fn digests(&self) -> rusqlite::Result<BTreeMap<String, String>> {
        self.computed_digests(None)
    }

    /// Контрольные суммы, которые дала бы проекция этих архивных записей:
    /// эталон для сверки индекса с архивом без изменения индекса.
    pub fn expected_digests<'a>(
        records: impl IntoIterator<Item = (&'a str, &'a str, &'a BenchmarkResult)>,
    ) -> rusqlite::Result<BTreeMap<String, String>> {
        let mut reference = Index::in_memory()?;
        for (id, path, result) in records {
            reference.insert(id, path, result)?;
        }
        reference.digests()
    }

    fn has_tables(&self) -> rusqlite::Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type = 'table'",
            [],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    /// Создать текущую схему, удалив прежние таблицы индекса.
    fn create(&mut self) -> rusqlite::Result<()> {
        // Внутри транзакции `PRAGMA foreign_keys` ничего не делает.
        self.conn.pragma_update(None, "foreign_keys", false)?;
        let tx = self.conn.transaction()?;
        let existing: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            )?;
            stmt.query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?
        };
        for name in TABLES.iter().map(|s| (*s).to_owned()).chain(existing) {
            tx.execute_batch(&format!(
                "DROP TABLE IF EXISTS \"{}\"",
                name.replace('"', "")
            ))?;
        }
        tx.execute_batch(SCHEMA)?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        self.conn.pragma_update(None, "foreign_keys", true)
    }

    pub fn ids(&self) -> rusqlite::Result<BTreeSet<String>> {
        let mut stmt = self.conn.prepare("SELECT id FROM results")?;
        stmt.query_map([], |r| r.get(0))?.collect()
    }

    /// Удалить строки записи из всех таблиц проекции, в том числе дочерние
    /// строки, оставшиеся без строки `results`.
    pub fn remove(&mut self, id: &str) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        for (table, owner, _) in PROJECTION.iter().rev() {
            tx.execute(&format!("DELETE FROM {table} WHERE {owner} = ?1"), [id])?;
        }
        tx.commit()
    }

    /// Добавить проекцию результата одной транзакцией; повтор — без изменений.
    pub fn insert(&mut self, id: &str, path: &str, r: &BenchmarkResult) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        let exists: Option<i64> = tx
            .query_row("SELECT 1 FROM results WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        if exists.is_some() {
            return Ok(());
        }
        let successful = r
            .runs
            .iter()
            .filter(|x| x.status == RunStatus::Success)
            .count();
        let code = &r.provenance.code;
        let tool = |t: &benchmark_contract::result::Tool| format!("{} {}", t.name, t.version);
        tx.execute(
            "INSERT INTO results VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, '')",
            params![
                id,
                path,
                r.schema_version,
                r.scenario.id,
                r.scenario.module,
                r.scenario.revision,
                r.scenario.kind.as_str(),
                r.profile.id,
                r.profile.cache.as_str(),
                r.profile.network.as_str(),
                text(&r.outcome.status),
                r.outcome.reason,
                text(&r.observation),
                r.runs.len() as i64,
                successful as i64,
                r.warmup_runs,
                r.provenance.recorded_at,
                time_key(&r.provenance.recorded_at),
                code.module_version,
                code.git_commit,
                code.dirty,
                code.source_tree_sha256,
                tool(&r.provenance.measurement),
                r.provenance.runner.as_ref().map(tool),
            ],
        )?;
        for input in &r.inputs {
            tx.execute(
                "INSERT INTO inputs VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    id,
                    input.id,
                    text(&input.role),
                    text(&input.identity.method),
                    input.identity.value
                ],
            )?;
            for (name, quantity) in &input.size {
                tx.execute(
                    "INSERT INTO input_sizes VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![id, input.id, name, quantity.value, quantity.unit.as_str()],
                )?;
            }
        }
        for (name, fact) in &r.environment {
            let (status, value) = match fact {
                Fact::Known { value } => ("known", Some(value)),
                Fact::Unavailable { reason } => ("unavailable", Some(reason)),
            };
            tx.execute(
                "INSERT INTO environment VALUES (?1, ?2, ?3, ?4)",
                params![id, name, status, value],
            )?;
        }
        for (name, value) in &r.parameters {
            tx.execute(
                "INSERT INTO parameters VALUES (?1, ?2, ?3)",
                params![id, name, value],
            )?;
        }
        for metric in &r.metrics {
            let aggregate =
                benchmark_contract::result::Aggregate::compute(&r.successful_values(&metric.key));
            tx.execute(
                "INSERT INTO metrics VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    id,
                    metric.key,
                    metric.metric.as_str(),
                    metric.scope.as_str(),
                    metric.source.as_str(),
                    metric.unit.as_str(),
                    metric.label,
                    text(&metric.availability),
                    metric.reason,
                    aggregate.map(|a| a.count),
                    aggregate.map(|a| a.mean),
                    aggregate.and_then(|a| a.stddev),
                    aggregate.map(|a| a.median),
                    aggregate.map(|a| a.min),
                    aggregate.map(|a| a.max),
                ],
            )?;
        }
        let digest = computed_digests(&tx, Some(id))?
            .remove(id)
            .expect("строка записи только что вставлена");
        tx.execute(
            &format!("UPDATE results SET {DIGEST_COLUMN} = ?2 WHERE id = ?1"),
            params![id, digest],
        )?;
        tx.commit()
    }

    /// Строки, удовлетворяющие фильтру, от новых к старым.
    pub fn query(&self, filter: &Filter) -> rusqlite::Result<Vec<Row>> {
        let mut sql = String::from(
            "SELECT id, recorded_at, scenario_id, revision, profile_id, outcome, outcome_reason, observation, runs, successful_runs, module_version, git_commit, dirty FROM results r WHERE 1 = 1",
        );
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        let mut bind = |sql: &mut String, clause: &str, value: Box<dyn rusqlite::ToSql>| {
            values.push(value);
            sql.push_str(&clause.replace('?', &format!("?{}", values.len())));
        };
        if let Some(v) = &filter.scenario {
            bind(&mut sql, " AND scenario_id = ?", Box::new(v.clone()));
        }
        if let Some(v) = &filter.module {
            bind(&mut sql, " AND module = ?", Box::new(v.clone()));
        }
        if let Some(v) = filter.revision {
            bind(&mut sql, " AND revision = ?", Box::new(v));
        }
        if let Some(v) = &filter.profile {
            bind(&mut sql, " AND profile_id = ?", Box::new(v.clone()));
        }
        if !filter.outcomes.is_empty() {
            let list: Vec<String> = filter
                .outcomes
                .iter()
                .map(|o| format!("'{}'", text(o)))
                .collect();
            sql.push_str(&format!(" AND outcome IN ({})", list.join(", ")));
        }
        if let Some(v) = &filter.since {
            bind(&mut sql, " AND recorded_key >= ?", Box::new(v.clone()));
        }
        if let Some(v) = &filter.until {
            bind(&mut sql, " AND recorded_key <= ?", Box::new(v.clone()));
        }
        if let Some(v) = &filter.module_version {
            bind(&mut sql, " AND module_version = ?", Box::new(v.clone()));
        }
        if let Some(v) = &filter.commit {
            bind(
                &mut sql,
                " AND substr(git_commit, 1, length(?)) = ?",
                Box::new(v.clone()),
            );
        }
        if let Some(v) = &filter.input_identity {
            bind(
                &mut sql,
                " AND EXISTS (SELECT 1 FROM inputs i WHERE i.result_id = r.id AND substr(i.identity, 1, length(?)) = ?)",
                Box::new(v.clone()),
            );
        }
        for (name, value) in &filter.environment {
            values.push(Box::new(name.clone()));
            let n = values.len();
            values.push(Box::new(value.clone()));
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM environment e WHERE e.result_id = r.id AND e.status = 'known' AND e.name = ?{n} AND e.value = ?{})",
                n + 1
            ));
        }
        sql.push_str(" ORDER BY recorded_key DESC, id");
        if let Some(limit) = filter.limit {
            sql.push_str(&format!(" LIMIT {limit}"));
        }
        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = values.iter().map(|v| v.as_ref()).collect();
        stmt.query_map(params.as_slice(), |r| {
            Ok(Row {
                id: r.get(0)?,
                recorded_at: r.get(1)?,
                scenario: r.get(2)?,
                revision: r.get(3)?,
                profile: r.get(4)?,
                outcome: r.get(5)?,
                outcome_reason: r.get(6)?,
                observation: r.get(7)?,
                runs: r.get(8)?,
                successful_runs: r.get(9)?,
                module_version: r.get(10)?,
                git_commit: r.get(11)?,
                dirty: r.get(12)?,
            })
        })?
        .collect()
    }

    /// Идентификаторы, начинающиеся с префикса.
    pub fn matching(&self, prefix: &str) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM results WHERE id LIKE ?1 || '%' ORDER BY id LIMIT 10")?;
        stmt.query_map([prefix], |r| r.get(0))?.collect()
    }

    /// Результаты той же серии «сценарий × профиль».
    pub fn series(&self, scenario: &str, profile: &str) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM results WHERE scenario_id = ?1 AND profile_id = ?2 ORDER BY recorded_key DESC, id",
        )?;
        stmt.query_map([scenario, profile], |r| r.get(0))?.collect()
    }

    pub fn count(&self) -> rusqlite::Result<i64> {
        self.conn
            .query_row("SELECT count(*) FROM results", [], |r| r.get(0))
    }
}

/// Контрольные суммы проекции по строкам таблиц; `only` — одна запись.
fn computed_digests(
    conn: &Connection,
    only: Option<&str>,
) -> rusqlite::Result<BTreeMap<String, String>> {
    let mut hashers: BTreeMap<String, Sha256> = BTreeMap::new();
    for (table, owner, order) in PROJECTION {
        let sql = match only {
            Some(_) => format!("SELECT * FROM {table} WHERE {owner} = ?1 ORDER BY {order}"),
            None => format!("SELECT * FROM {table} ORDER BY {owner}, {order}"),
        };
        let mut stmt = conn.prepare(&sql)?;
        let names: Vec<String> = stmt.column_names().map(str::to_owned).collect();
        let owner_at = names
            .iter()
            .position(|n| n == owner)
            .expect("колонка записи");
        let params: Vec<&dyn rusqlite::ToSql> = match &only {
            Some(id) => vec![id],
            None => Vec::new(),
        };
        let mut rows = stmt.query(params.as_slice())?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(owner_at)?;
            let hasher = hashers.entry(id).or_default();
            feed(hasher, table.as_bytes());
            for (n, name) in names.iter().enumerate() {
                if name == DIGEST_COLUMN {
                    continue;
                }
                feed(hasher, name.as_bytes());
                match row.get_ref(n)? {
                    rusqlite::types::ValueRef::Null => feed(hasher, b"n"),
                    rusqlite::types::ValueRef::Integer(v) => {
                        feed(hasher, b"i");
                        feed(hasher, &v.to_le_bytes());
                    }
                    rusqlite::types::ValueRef::Real(v) => {
                        feed(hasher, b"r");
                        feed(hasher, &v.to_bits().to_le_bytes());
                    }
                    rusqlite::types::ValueRef::Text(v) => {
                        feed(hasher, b"t");
                        feed(hasher, v);
                    }
                    rusqlite::types::ValueRef::Blob(v) => {
                        feed(hasher, b"b");
                        feed(hasher, v);
                    }
                }
            }
        }
    }
    Ok(hashers
        .into_iter()
        .map(|(id, h)| (id, crate::archive::hex(&h.finalize())))
        .collect())
}

/// Поле контрольной суммы с длиной: границы значений однозначны.
fn feed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn join(names: &[&String]) -> String {
    names
        .iter()
        .map(|n| n.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub scenario: Option<String>,
    pub module: Option<String>,
    pub revision: Option<u32>,
    pub profile: Option<String>,
    pub outcomes: Vec<OutcomeStatus>,
    /// Нормализованные [`time_key`] границы, включительно.
    pub since: Option<String>,
    pub until: Option<String>,
    pub module_version: Option<String>,
    pub commit: Option<String>,
    pub input_identity: Option<String>,
    pub environment: Vec<(String, String)>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub id: String,
    pub recorded_at: String,
    pub scenario: String,
    pub revision: u32,
    pub profile: String,
    pub outcome: String,
    pub outcome_reason: Option<String>,
    pub observation: String,
    pub runs: u32,
    pub successful_runs: u32,
    pub module_version: String,
    pub git_commit: Option<String>,
    pub dirty: Option<bool>,
}

/// Граница фильтра времени: дата `YYYY-MM-DD` или метка RFC 3339 UTC.
/// Дата в `until` включает весь день.
pub fn bound(text: &str, end_of_day: bool) -> Result<String, String> {
    let full = if text.len() == 10 {
        format!(
            "{text}T{}Z",
            if end_of_day {
                "23:59:60.999999999"
            } else {
                "00:00:00"
            }
        )
    } else {
        text.to_owned()
    };
    if !benchmark_contract::result::is_utc_timestamp(&full) {
        return Err(format!(
            "граница времени {text}: ожидается YYYY-MM-DD или YYYY-MM-DDTHH:MM:SS[.дробь]Z"
        ));
    }
    Ok(time_key(&full))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_keys_sort_chronologically() {
        let a = time_key("2026-10-28T00:00:00Z");
        let b = time_key("2026-10-28T00:00:00.5Z");
        let c = time_key("2026-10-28T00:00:01Z");
        assert!(a < b && b < c, "{a} {b} {c}");
        assert!(bound("2026-10-28", true).unwrap() > c);
        assert!(bound("2026-10-28", false).unwrap() <= a);
        assert!(bound("28.10.2026", false).is_err());
    }
}
