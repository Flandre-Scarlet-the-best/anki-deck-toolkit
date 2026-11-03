//! Структурированная диагностика долгих пакетных операций.
//!
//! Модуль принимает уже безопасно открытый runtime-файл. Путь и namespace
//! принадлежат `SafeBatchRuntime`; здесь настраиваются только слои `tracing`,
//! формат и гарантированное завершение записи.

use std::fs::File;
use std::io::{self, IsTerminal, Write};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use tracing::Dispatch;
use tracing_appender::non_blocking::{ErrorCounter, NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{Layer, fmt};
use url::Url;

const DIAGNOSTIC_BUFFERED_LINES: usize = 1024;
const FLUSH_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_MESSAGE_BYTES: usize = 4096;
const MAX_ROUTE_BYTES: usize = 512;

/// Режим вывода CLI, определяющий наличие человекочитаемого слоя в `stderr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Human,
    Json,
}

/// Сохраняет диспетчер и рабочий поток до конца одного пакетного запуска.
#[must_use = "guard должен жить до конца run, чтобы сбросить диагностический файл"]
pub struct RunLogGuard {
    dispatch: Option<Dispatch>,
    worker: Option<WorkerGuard>,
    progress: Arc<WriteProgress>,
    error_counter: ErrorCounter,
}

impl RunLogGuard {
    /// Настраивает файловый слой NDJSON поверх файла, безопасно открытого runtime.
    pub fn new(file: File, mode: OutputMode) -> Self {
        Self::with_buffered_lines_and_terminal_writer(
            file,
            mode,
            DIAGNOSTIC_BUFFERED_LINES,
            io::stderr,
        )
    }

    #[cfg(test)]
    fn with_buffered_lines(file: File, mode: OutputMode, buffered_lines: usize) -> Self {
        Self::with_buffered_lines_and_terminal_writer(file, mode, buffered_lines, io::stderr)
    }

    fn with_buffered_lines_and_terminal_writer<W>(
        file: File,
        mode: OutputMode,
        buffered_lines: usize,
        terminal_writer: W,
    ) -> Self
    where
        W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
    {
        let progress = Arc::new(WriteProgress::default());
        let checked_file = CheckedFileWriter {
            file,
            progress: Arc::clone(&progress),
        };
        let (non_blocking, worker) = NonBlockingBuilder::default()
            .buffered_lines_limit(buffered_lines.max(1))
            .lossy(false)
            .thread_name("asset-store-diagnostic-log")
            .finish(checked_file);
        let error_counter = non_blocking.error_counter();
        let file_layer = fmt::layer()
            .json()
            .with_ansi(false)
            .with_target(true)
            .with_current_span(true)
            .with_span_list(true)
            .with_writer(CountedNonBlocking {
                writer: non_blocking,
                progress: Arc::clone(&progress),
            });
        let terminal_layer = match mode {
            OutputMode::Human => Some(
                fmt::layer()
                    .pretty()
                    .with_ansi(io::stderr().is_terminal())
                    .with_writer(terminal_writer)
                    .with_filter(LevelFilter::INFO),
            ),
            OutputMode::Json => None,
        };
        let subscriber = tracing_subscriber::registry()
            .with(file_layer)
            .with(terminal_layer);
        Self {
            dispatch: Some(Dispatch::new(subscriber)),
            worker: Some(worker),
            progress,
            error_counter,
        }
    }

    /// Выполняет синхронную работу с dispatcher этого запуска.
    pub fn with_default<T>(&self, operation: impl FnOnce() -> T) -> T {
        let dispatch = self
            .dispatch
            .as_ref()
            .expect("dispatcher жив до завершения RunLogGuard");
        tracing::dispatcher::with_default(dispatch, operation)
    }

    /// Возвращает dispatcher для асинхронного future через `WithSubscriber`.
    pub fn dispatch(&self) -> Dispatch {
        self.dispatch
            .as_ref()
            .expect("dispatcher жив до завершения RunLogGuard")
            .clone()
    }

    /// Закрывает dispatcher, ждёт обработки всех поставленных строк и проверяет ошибки записи.
    pub fn finish(mut self) -> Result<(), String> {
        self.dispatch.take();
        let target = self.progress.lock().enqueued;
        let mut state = self.progress.lock();
        let (next, _) = self
            .progress
            .changed
            // После ошибки записи tracing-appender не вызывает flush до Shutdown:
            // ждать дальше бессмысленно, известная I/O-ошибка важнее таймаута.
            .wait_timeout_while(state, FLUSH_TIMEOUT, |state| {
                state.flushed < target && state.error.is_none()
            })
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state = next;
        let flush_error = (state.flushed < target && state.error.is_none()).then(|| {
            format!(
                "истёк срок ожидания записи диагностического файла: обработано {} из {target} строк",
                state.flushed
            )
        });
        drop(state);

        // WorkerGuard сначала отправляет Shutdown, после чего его worker вызывает
        // flush у файлового writer. Счётчик выше подтверждает, что все события run
        // обработаны ещё до этой границы.
        drop(self.worker.take());

        if let Some(error) = self.progress.lock().error.clone() {
            return Err(format!("ошибка записи диагностического файла: {error}"));
        }
        if let Some(error) = flush_error {
            return Err(error);
        }
        let dropped = self.error_counter.dropped_lines();
        if dropped != 0 {
            return Err(format!(
                "tracing-appender сообщил о потере {dropped} диагностических строк"
            ));
        }
        Ok(())
    }
}

impl Drop for RunLogGuard {
    fn drop(&mut self) {
        drop(self.dispatch.take());
        drop(self.worker.take());
    }
}

#[derive(Default)]
struct ProgressState {
    enqueued: u64,
    processed: u64,
    flushed: u64,
    error: Option<String>,
}

#[derive(Default)]
struct WriteProgress {
    state: Mutex<ProgressState>,
    changed: Condvar,
}

impl WriteProgress {
    fn lock(&self) -> MutexGuard<'_, ProgressState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn record_enqueue(&self) {
        let mut state = self.lock();
        state.enqueued = state.enqueued.saturating_add(1);
        self.changed.notify_all();
    }

    fn record_enqueue_error(&self, error: &io::Error) {
        let mut state = self.lock();
        state.error.get_or_insert_with(|| error.to_string());
        self.changed.notify_all();
    }

    fn record_processed(&self, result: &io::Result<()>) {
        let mut state = self.lock();
        state.processed = state.processed.saturating_add(1);
        if let Err(error) = result {
            state.error.get_or_insert_with(|| error.to_string());
        }
        self.changed.notify_all();
    }

    fn record_flush(&self, result: &io::Result<()>) {
        let mut state = self.lock();
        state.flushed = state.processed;
        if let Err(error) = result {
            state.error.get_or_insert_with(|| error.to_string());
        }
        self.changed.notify_all();
    }
}

#[derive(Clone)]
struct CountedNonBlocking {
    writer: NonBlocking,
    progress: Arc<WriteProgress>,
}

impl Write for CountedNonBlocking {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self.writer.write(bytes) {
            Ok(written) => {
                self.progress.record_enqueue();
                Ok(written)
            }
            Err(error) => {
                self.progress.record_enqueue_error(&error);
                Err(error)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

impl<'a> MakeWriter<'a> for CountedNonBlocking {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

struct CheckedFileWriter {
    file: File,
    progress: Arc<WriteProgress>,
}

impl Write for CheckedFileWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.file.write(bytes)
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        let normalized = nullable_generation_event(bytes);
        let result = self.file.write_all(normalized.as_deref().unwrap_or(bytes));
        self.progress.record_processed(&result);
        result
    }

    fn flush(&mut self) -> io::Result<()> {
        let result = self.file.flush();
        self.progress.record_flush(&result);
        result
    }
}

/// `tracing` опускает поле `Option::None`; для схемы получения сохраняем
/// явное `null` в необязательном поле `generation` JSONL-файла.
fn nullable_generation_event(line: &[u8]) -> Option<Vec<u8>> {
    const SCHEMA: &[u8] = b"browser_acquisition_v1";
    if !line.windows(SCHEMA.len()).any(|window| window == SCHEMA) {
        return None;
    }

    let mut event: serde_json::Value = serde_json::from_slice(line).ok()?;
    let fields = event.get_mut("fields")?.as_object_mut()?;
    if fields.get("schema")?.as_str()? != "browser_acquisition_v1"
        || fields.contains_key("generation")
    {
        return None;
    }
    fields.insert("generation".to_owned(), serde_json::Value::Null);

    let mut normalized = serde_json::to_vec(&event).ok()?;
    if line.ends_with(b"\n") {
        normalized.push(b'\n');
    }
    Some(normalized)
}

/// Удаляет credentials, query/fragment и управляющие символы из URL/маршрута.
pub fn safe_route(value: &str) -> String {
    let candidate = value.trim();
    let route = if let Ok(mut url) = Url::parse(candidate) {
        if !matches!(url.scheme(), "http" | "https") {
            return "[маршрут опущен]".into();
        }
        let _ = url.set_username("");
        let _ = url.set_password(None);
        url.set_query(None);
        url.set_fragment(None);
        url.to_string()
    } else {
        candidate
            .split(['?', '#'])
            .next()
            .unwrap_or_default()
            .to_owned()
    };
    bound_text(&strip_controls(&route), MAX_ROUTE_BYTES)
}

/// Возвращает ограниченный и очищенный текст ошибки для безопасного лога.
/// Сохранённая типизированная ошибка не меняется: очищается только её копия в журнале.
pub fn safe_message(message: &str) -> String {
    let without_ansi = strip_ansi(message);
    let without_controls = strip_controls(&without_ansi);
    if looks_like_markup(&without_controls) {
        return "[HTML/DOM фрагмент опущен]".into();
    }
    let without_secrets = redact_assignments(&without_controls);
    let without_url_secrets = redact_urls(&without_secrets);
    bound_text(&without_url_secrets, MAX_MESSAGE_BYTES)
}

fn strip_controls(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '\n' | '\r' | '\t' => ' ',
            character if character.is_control() => ' ',
            character => character,
        })
        .collect()
}

fn strip_ansi(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut output = String::with_capacity(value.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] != '\u{1b}' {
            output.push(chars[index]);
            index += 1;
            continue;
        }
        index += 1;
        if index >= chars.len() {
            break;
        }
        match chars[index] {
            '[' => {
                index += 1;
                while index < chars.len() {
                    let character = chars[index];
                    index += 1;
                    if ('@'..='~').contains(&character) {
                        break;
                    }
                }
            }
            ']' => {
                index += 1;
                while index < chars.len() {
                    if chars[index] == '\u{7}' {
                        index += 1;
                        break;
                    }
                    if chars[index] == '\u{1b}'
                        && chars
                            .get(index + 1)
                            .is_some_and(|character| *character == '\\')
                    {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
            }
            _ => index += 1,
        }
    }
    output
}

fn looks_like_markup(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if ["<!doctype", "<html", "<body", "<script", "<svg"]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return true;
    }
    let bytes = value.as_bytes();
    let mut tags = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'<' {
            continue;
        }
        let mut cursor = index + 1;
        if bytes.get(cursor) == Some(&b'/') {
            cursor += 1;
        }
        if bytes
            .get(cursor)
            .is_some_and(|character| character.is_ascii_alphabetic())
            && bytes[cursor..]
                .iter()
                .take(256)
                .any(|character| *character == b'>')
        {
            tags += 1;
            if tags >= 2 {
                return true;
            }
        }
    }
    false
}

fn redact_assignments(value: &str) -> String {
    const MARKERS: &[&str] = &[
        "authorization",
        "set-cookie",
        "cookie",
        "api_key",
        "apikey",
        "access_token",
        "refresh_token",
        "password",
        "secret",
        "csrf",
        "token",
    ];
    let lowered: Vec<u8> = value
        .bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .collect();
    let bytes = value.as_bytes();
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    while cursor < bytes.len() {
        let match_at = MARKERS
            .iter()
            .filter_map(|marker| {
                lowered[cursor..]
                    .windows(marker.len())
                    .position(|window| window == marker.as_bytes())
                    .map(|offset| (cursor + offset, *marker))
            })
            .min_by_key(|(index, _)| *index);
        let Some((marker_at, marker)) = match_at else {
            output.push_str(&value[cursor..]);
            break;
        };
        output.push_str(&value[cursor..marker_at]);
        let mut value_at = marker_at + marker.len();
        while value_at < bytes.len() && bytes[value_at].is_ascii_whitespace() {
            value_at += 1;
        }
        if marker_at > 0
            && matches!(bytes.get(marker_at - 1), Some(b'\'' | b'"'))
            && bytes.get(value_at) == bytes.get(marker_at - 1)
        {
            // Маркер внутри JSON-ключа: перед ним уже стоит открывающая кавычка,
            // а следующая кавычка закрывает ключ.
            value_at += 1;
            while value_at < bytes.len() && bytes[value_at].is_ascii_whitespace() {
                value_at += 1;
            }
        }
        if !matches!(bytes.get(value_at), Some(b'=' | b':')) {
            output.push_str(&value[marker_at..marker_at + marker.len()]);
            cursor = marker_at + marker.len();
            continue;
        }
        value_at += 1;
        while value_at < bytes.len() && bytes[value_at].is_ascii_whitespace() {
            value_at += 1;
        }
        output.push_str(&value[marker_at..value_at]);
        if matches!(bytes.get(value_at), Some(b'\'' | b'"')) {
            let quote = bytes[value_at];
            output.push(quote as char);
            output.push_str("[СКРЫТО]");
            let mut value_end = value_at + 1;
            let mut escaped = false;
            while value_end < bytes.len() {
                if escaped {
                    escaped = false;
                } else if bytes[value_end] == b'\\' {
                    escaped = true;
                } else if bytes[value_end] == quote {
                    break;
                }
                value_end += 1;
            }
            if value_end < bytes.len() {
                output.push(quote as char);
                cursor = value_end + 1;
            } else {
                cursor = value_end;
            }
        } else {
            output.push_str("[СКРЫТО]");
            cursor = value_at;
            if matches!(marker, "cookie" | "set-cookie" | "authorization") {
                // Незаключённый в кавычки заголовок может содержать несколько
                // значений через `;` или пробелы. Скрываем остаток сообщения.
                cursor = bytes.len();
            } else {
                while cursor < bytes.len()
                    && !bytes[cursor].is_ascii_whitespace()
                    && !matches!(bytes[cursor], b',' | b';' | b'&' | b'}' | b'"' | b'\'')
                {
                    cursor += 1;
                }
            }
        }
    }
    output
}

fn redact_urls(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for token in value.split_inclusive(char::is_whitespace) {
        let (word, whitespace) = token
            .find(char::is_whitespace)
            .map_or((token, ""), |index| token.split_at(index));
        output.push_str(&redact_url_token(word));
        output.push_str(whitespace);
    }
    output
}

fn redact_url_token(token: &str) -> String {
    let Some(start) = token.find("https://").or_else(|| token.find("http://")) else {
        return token.to_owned();
    };
    let prefix = &token[..start];
    let tail = &token[start..];
    let suffix_start = tail
        .char_indices()
        .rev()
        .find(|(_, character)| {
            !matches!(
                character,
                '.' | ',' | ';' | ':' | ')' | ']' | '}' | '"' | '\''
            )
        })
        .map_or(0, |(index, character)| index + character.len_utf8());
    let (url_text, suffix) = tail.split_at(suffix_start);
    format!("{prefix}{}{suffix}", safe_route(url_text))
}

fn bound_text(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let end = value.floor_char_boundary(maximum.saturating_sub('…'.len_utf8()));
    format!("{}…", &value[..end])
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use serde_json::Value;
    use tracing_subscriber::fmt::MakeWriter;

    use super::{OutputMode, RunLogGuard, safe_message, safe_route};
    use crate::temp_workspace::TempWorkspace;

    struct TemporaryDirectory {
        workspace: TempWorkspace,
    }

    impl TemporaryDirectory {
        fn new() -> Self {
            Self {
                workspace: TempWorkspace::create("asset-store-diagnostics-tests").unwrap(),
            }
        }

        fn path(&self) -> &std::path::Path {
            self.workspace.path()
        }
    }

    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    struct SharedBufferWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBufferWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let mut buffer = self.0.lock().unwrap();
            buffer.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> MakeWriter<'writer> for SharedBuffer {
        type Writer = SharedBufferWriter;

        fn make_writer(&'writer self) -> Self::Writer {
            SharedBufferWriter(Arc::clone(&self.0))
        }
    }

    #[test]
    fn file_layer_is_valid_ndjson_without_ansi_and_flushes_before_finish_returns() {
        let temporary = TemporaryDirectory::new();
        let path = temporary.path().join("run.jsonl");
        let file = fs::File::create(&path).unwrap();
        let logger = RunLogGuard::with_buffered_lines(file, OutputMode::Json, 1);
        logger.with_default(|| {
            for event_index in 0..128 {
                tracing::info!(
                    operation = "batch_run",
                    batch_id = "synthetic",
                    run_id = "synthetic-run",
                    event_index,
                    "контрольная точка завершена"
                );
            }
        });
        logger.finish().unwrap();

        let content = fs::read_to_string(path).unwrap();
        assert!(!content.contains('\u{1b}'));
        let events = content
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(events.len(), 128);
        assert!(events.iter().all(|event| event["timestamp"].is_string()));
        assert!(events.iter().all(|event| event["target"].is_string()));
        assert!(
            events
                .iter()
                .all(|event| event["fields"]["operation"] == "batch_run")
        );
        assert!(
            events
                .iter()
                .all(|event| event["fields"]["batch_id"] == "synthetic")
        );
    }

    #[test]
    fn write_error_is_reported_instead_of_flush_timeout() {
        // /dev/full даёт настоящий ENOSPC на каждую запись.
        let Ok(file) = fs::OpenOptions::new().write(true).open("/dev/full") else {
            return;
        };
        let logger = RunLogGuard::with_buffered_lines(file, OutputMode::Json, 1);
        logger.with_default(|| {
            tracing::info!(operation = "batch_run", "контрольная точка завершена");
        });
        let started = std::time::Instant::now();
        let error = logger.finish().unwrap_err();
        // Известная ошибка записи не должна дожидаться срока сброса.
        assert!(started.elapsed() < super::FLUSH_TIMEOUT);
        assert!(
            error.starts_with("ошибка записи диагностического файла"),
            "{error}"
        );
        assert!(!error.contains("истёк срок"), "{error}");
    }

    #[test]
    fn safe_route_removes_credentials_query_and_fragment() {
        assert_eq!(
            safe_route("https://user:secret@jpdb.io/search?q=敏感&token=secret#results"),
            "https://jpdb.io/search"
        );
        assert_eq!(safe_route("/search?q=敏感#results"), "/search");
    }

    #[test]
    fn safe_message_bounds_and_redacts_secrets_and_markup() {
        let message = safe_message(
            "запрос завершился ошибкой на https://user:secret@jpdb.io/search?token=hidden#x Authorization: Bearer credential",
        );
        assert!(!message.contains("secret"));
        assert!(!message.contains("hidden"));
        assert!(!message.contains("credential"));
        assert!(message.contains("https://jpdb.io/search"));
        assert!(
            !safe_message("<html><body>закрытые данные</body></html>").contains("закрытые данные")
        );
        assert!(safe_message(&"日".repeat(5000)).len() <= 4096);
    }

    #[test]
    fn safe_message_redacts_quoted_json_keys_and_complete_cookie_headers() {
        let message =
            safe_message(r#"{"Authorization" : "Bearer json-secret", "token" : "token-secret"}"#);
        assert!(!message.contains("json-secret"));
        assert!(!message.contains("token-secret"));
        assert!(message.contains("[СКРЫТО]"));

        let cookie =
            safe_message("запрос завершился ошибкой Cookie: first=hidden; second=also-hidden");
        assert!(!cookie.contains("first=hidden"));
        assert!(!cookie.contains("second=also-hidden"));
    }

    #[test]
    fn human_terminal_layer_writes_to_stderr_sink_and_json_mode_suppresses_it() {
        let temporary = TemporaryDirectory::new();
        let stdout = SharedBuffer::default();
        let stderr = SharedBuffer::default();
        let human_log = fs::File::create(temporary.path().join("human.jsonl")).unwrap();
        let logger = RunLogGuard::with_buffered_lines_and_terminal_writer(
            human_log,
            OutputMode::Human,
            16,
            stderr.clone(),
        );
        logger.with_default(|| tracing::info!(event = "terminal_probe", "проверка вывода ошибок"));
        logger.finish().unwrap();
        let terminal = String::from_utf8(stderr.0.lock().unwrap().clone()).unwrap();
        assert!(terminal.contains("проверка вывода ошибок"));
        assert!(stdout.0.lock().unwrap().is_empty());

        let json_stderr = SharedBuffer::default();
        let json_log = fs::File::create(temporary.path().join("json.jsonl")).unwrap();
        let logger = RunLogGuard::with_buffered_lines_and_terminal_writer(
            json_log,
            OutputMode::Json,
            16,
            json_stderr.clone(),
        );
        logger.with_default(|| tracing::info!(event = "file_only_probe", "только в файл"));
        logger.finish().unwrap();
        assert!(json_stderr.0.lock().unwrap().is_empty());
    }
}
