//! Локальный HTTPS fixture-сайт, который тестовый Chromium видит как
//! `https://forvo.com` (через `--host-resolver-rules`). Разметка повторяет только
//! проверенный контракт `dom.js`/`auth.js`; байты — синтетические MP3 из ffmpeg.
//! Сценарий управляет ответами загрузок: задержать заголовки, отдать часть тела
//! и ждать, оборвать соединение, отдать мусор.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

pub const WORD: &str = "元気";
pub const READING: &str = "げんき";
pub const RECORD_A: &str = "3661006";
pub const RECORD_B: &str = "5036880";
/// Запись, у которой кнопка ведёт на адрес другой записи.
pub const RECORD_WRONG_HREF: &str = "7000001";
pub const WORD_DOM_CHANGED: &str = "別語";
pub const WORD_OTHER_LANGUAGE: &str = "英語";
pub const WORD_PHRASES: &str = "句語";
pub const WORD_LOGIN: &str = "未認証";
pub const WORD_MISMATCHED_BUTTON: &str = "誤記";
pub const WORD_WRONG_HREF: &str = "誤先";
pub const USER_HOST: &str = "user-files.test";

/// Поведение ответа на загрузку записи.
#[derive(Clone)]
pub enum Behavior {
    Normal,
    /// Заголовки не отправляются, пока ворота закрыты.
    HoldHeaders(watch::Receiver<bool>),
    /// Заголовки и половина тела, остаток — после открытия ворот.
    HoldBody(watch::Receiver<bool>),
    /// Заголовки и половина тела, затем соединение обрывается.
    Abort,
    /// Полный ответ, но байты не MP3.
    Garbage,
}

pub struct Gate {
    tx: watch::Sender<bool>,
}
impl Gate {
    pub fn new() -> (Gate, watch::Receiver<bool>) {
        let (tx, rx) = watch::channel(false);
        (Gate { tx }, rx)
    }
    pub fn open(&self) {
        let _ = self.tx.send(true);
    }
}

#[derive(Default)]
struct Control {
    behaviors: HashMap<String, Behavior>,
    log: Vec<String>,
}

#[derive(Clone)]
pub struct Fixture {
    pub port: u16,
    control: Arc<Mutex<Control>>,
    pub mp3_a: Arc<Vec<u8>>,
    pub mp3_b: Arc<Vec<u8>>,
    pub user_file: Arc<Vec<u8>>,
    stop: watch::Sender<bool>,
}

fn lock(control: &Mutex<Control>) -> std::sync::MutexGuard<'_, Control> {
    control
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Fixture {
    pub fn set(&self, record: &str, behavior: Behavior) {
        lock(&self.control)
            .behaviors
            .insert(record.to_owned(), behavior);
    }
    pub fn reset(&self) {
        let mut control = lock(&self.control);
        control.behaviors.clear();
        control.log.clear();
    }
    /// Журнал путей запросов (без заголовков и тел).
    pub fn log(&self) -> Vec<String> {
        lock(&self.control).log.clone()
    }
    pub fn requests_for(&self, record: &str) -> usize {
        let suffix = format!("/ja/{record}");
        self.log()
            .iter()
            .filter(|p| {
                p.starts_with("/download/mp3/") && p.trim_end_matches('/').ends_with(&suffix)
            })
            .count()
    }
    pub async fn wait_request(&self, record: &str, count: usize, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if self.requests_for(record) >= count {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }
    pub fn bytes_for(&self, record: &str) -> Arc<Vec<u8>> {
        if record == RECORD_B {
            self.mp3_b.clone()
        } else {
            self.mp3_a.clone()
        }
    }
    pub fn shutdown(&self) {
        let _ = self.stop.send(true);
    }

    pub async fn start(
        cert: &Path,
        key: &Path,
        mp3_a: Vec<u8>,
        mp3_b: Vec<u8>,
    ) -> Result<Fixture, String> {
        let certs = CertificateDer::pem_file_iter(cert)
            .map_err(|e| format!("сертификат fixture: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("сертификат fixture: {e}"))?;
        let key = PrivateKeyDer::from_pem_file(key).map_err(|e| format!("ключ fixture: {e}"))?;
        let config = ServerConfig::builder_with_provider(Arc::new(
            tokio_rustls::rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("TLS fixture: {e}"))?;
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| format!("порт fixture: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let (stop, mut stopped) = watch::channel(false);
        let fixture = Fixture {
            port,
            control: Arc::new(Mutex::new(Control::default())),
            mp3_a: Arc::new(mp3_a),
            mp3_b: Arc::new(mp3_b),
            user_file: Arc::new(b"user notes: synthetic file, not audio\n".repeat(64)),
            stop,
        };
        let served = fixture.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { continue };
                        let acceptor = acceptor.clone();
                        let fixture = served.clone();
                        tokio::spawn(async move {
                            if let Ok(Ok(tls)) = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(stream)).await {
                                let _ = fixture.serve(tls).await;
                            }
                        });
                    }
                    _ = stopped.changed() => break,
                }
            }
        });
        Ok(fixture)
    }

    async fn serve<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
        &self,
        mut stream: S,
    ) -> std::io::Result<()> {
        let mut head = Vec::new();
        let mut buf = [0_u8; 2048];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf))
                .await
                .map_err(|_| std::io::Error::other("timeout"))??;
            if n == 0 || head.len() > 16 * 1024 {
                return Ok(());
            }
            head.extend_from_slice(&buf[..n]);
        }
        let text = String::from_utf8_lossy(&head);
        let mut lines = text.split("\r\n");
        let request = lines.next().unwrap_or_default();
        let host = lines
            .find_map(|l| {
                l.strip_prefix("Host: ")
                    .or_else(|| l.strip_prefix("host: "))
            })
            .unwrap_or_default()
            .to_owned();
        let mut parts = request.split(' ');
        let (method, raw) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
        let path = raw.split('?').next().unwrap_or("/").to_owned();
        lock(&self.control).log.push(path.clone());
        if method != "GET" {
            return respond(&mut stream, 405, "text/plain", &[], b"method").await;
        }
        if host.starts_with(USER_HOST) {
            return respond(
                &mut stream,
                200,
                "application/octet-stream",
                &[(
                    "Content-Disposition",
                    "attachment; filename=\"user-notes.bin\"",
                )],
                &self.user_file,
            )
            .await;
        }
        let decoded = percent_decode(&path);
        let segments: Vec<&str> = decoded.split('/').filter(|s| !s.is_empty()).collect();
        match segments.as_slice() {
            ["word", word] => match word_page(word) {
                Some(html) => {
                    respond(
                        &mut stream,
                        200,
                        "text/html; charset=utf-8",
                        &[],
                        html.as_bytes(),
                    )
                    .await
                }
                None => {
                    respond(
                        &mut stream,
                        404,
                        "text/html; charset=utf-8",
                        &[],
                        b"<html><body>404</body></html>",
                    )
                    .await
                }
            },
            ["languages", ..] | [] => {
                respond(
                    &mut stream,
                    200,
                    "text/html; charset=utf-8",
                    &[],
                    home_page().as_bytes(),
                )
                .await
            }
            ["download", "mp3", _word, "ja", record] => self.download(&mut stream, record).await,
            _ => respond(&mut stream, 404, "text/plain", &[], b"not found").await,
        }
    }

    async fn download<S: tokio::io::AsyncWrite + Unpin>(
        &self,
        stream: &mut S,
        record: &str,
    ) -> std::io::Result<()> {
        let behavior = lock(&self.control)
            .behaviors
            .get(record)
            .cloned()
            .unwrap_or(Behavior::Normal);
        let bytes = match behavior {
            Behavior::Garbage => Arc::new(vec![0x5a; 4096]),
            _ => self.bytes_for(record),
        };
        let disposition = format!("attachment; filename=\"pronunciation_ja_{record}.mp3\"");
        if let Behavior::HoldHeaders(mut rx) = behavior.clone() {
            let _ = rx.wait_for(|open| *open).await;
        }
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: {}\r\nContent-Disposition: {disposition}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
            bytes.len()
        );
        stream.write_all(header.as_bytes()).await?;
        let half = bytes.len() / 2;
        match behavior {
            Behavior::HoldBody(mut rx) => {
                stream.write_all(&bytes[..half]).await?;
                stream.flush().await?;
                let _ = rx.wait_for(|open| *open).await;
                stream.write_all(&bytes[half..]).await?;
            }
            Behavior::Abort => {
                stream.write_all(&bytes[..half]).await?;
                stream.flush().await?;
                tokio::time::sleep(Duration::from_millis(300)).await;
                return Ok(());
            }
            _ => stream.write_all(&bytes).await?,
        }
        stream.flush().await?;
        stream.shutdown().await
    }
}

async fn respond<S: tokio::io::AsyncWrite + Unpin>(
    stream: &mut S,
    status: u16,
    content_type: &str,
    extra: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Error",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    stream.shutdown().await
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(v) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

const ACCOUNT: &str = r#"<div id="header"><div id="nav_account"><span class="sign_out"><a href="https://forvo.com/logout/">Logout</a></span></div></div>"#;

fn home_page() -> String {
    format!("<!doctype html><meta charset=utf-8><title>fixture</title>{ACCOUNT}<p>fixture home</p>")
}

struct Row<'a> {
    id: &'a str,
    votes: i32,
    speaker: &'a str,
    button_word: &'a str,
    button_record: &'a str,
    href_record: &'a str,
}

fn row(surface: &str, r: &Row) -> String {
    format!(
        r#"<li class="pronunciation"><div class="play" id="play_{id}">{surface}</div><span class="info"><span data-p2="speaker-{id}">speaker</span></span><span class="from">({speaker})</span><div class="more"><span class="pronunciationMoreMenu"></span><p class="votes_ja_content" id="word_rate_{id}"><span class="num_votes">{votes} votes</span></p><p class="download"><a href="/download/mp3/{href_word}/ja/{href}" data-p2="{word}" data-p3="ja" data-p4="{button}">Download MP3</a></p></div></li>"#,
        id = r.id,
        speaker = r.speaker,
        votes = r.votes,
        href_word = encode(surface),
        href = r.href_record,
        word = encode(r.button_word),
        button = r.button_record,
    )
}

fn ja_section(surface: &str, rows: &[Row]) -> String {
    let items: String = rows.iter().map(|r| row(surface, r)).collect();
    format!(
        r#"<div id="language-container-ja" class="language-container"><article id="language-ja" class="pronunciations"><header class="header-pronunciation"><em>{surface} pronunciation in <a href="https://forvo.com/languages/ja/">Japanese</a></em></header><ul id="pronunciations-list-ja" class="pronunciations-list">{items}</ul></article></div>"#
    )
}

fn page(body: &str, account: bool) -> String {
    let header = if account {
        ACCOUNT
    } else {
        r#"<div id="header"><a class="login" href="https://forvo.com/login/">Log in</a></div>"#
    };
    format!(
        "<!doctype html><html lang=en><head><meta charset=utf-8><title>fixture word</title></head><body>{header}<div class=\"page_word\"><section class=\"main_section\">{body}</section></div></body></html>"
    )
}

fn word_page(word: &str) -> Option<String> {
    let word: &'static str = [
        WORD,
        WORD_LOGIN,
        WORD_DOM_CHANGED,
        WORD_OTHER_LANGUAGE,
        WORD_PHRASES,
        WORD_MISMATCHED_BUTTON,
        WORD_WRONG_HREF,
    ]
    .into_iter()
    .find(|w| *w == word)?;
    let normal = |surface: &'static str| {
        vec![
            Row {
                id: RECORD_A,
                votes: 5,
                speaker: "Female from Japan",
                button_word: surface,
                button_record: RECORD_A,
                href_record: RECORD_A,
            },
            Row {
                id: RECORD_B,
                votes: 1,
                speaker: "Male from Japan",
                button_word: surface,
                button_record: RECORD_B,
                href_record: RECORD_B,
            },
        ]
    };
    Some(match word {
        WORD => page(&ja_section(word, &normal(word)), true),
        WORD_LOGIN => page(&ja_section(word, &normal(word)), false),
        WORD_DOM_CHANGED => page(
            r#"<div id="language-container-ja" class="language-container"><article id="language-ja" class="pronunciations"><header class="header-pronunciation"><a href="https://forvo.com/languages/ja/">Japanese</a></header><div class="new-layout"></div></article></div>"#,
            true,
        ),
        WORD_OTHER_LANGUAGE => page(
            r#"<div id="language-container-en" class="language-container"><article id="language-en" class="pronunciations"><header class="header-pronunciation"><em>English</em></header><ul id="pronunciations-list-en" class="pronunciations-list"></ul></article></div>"#,
            true,
        ),
        WORD_PHRASES => page(
            r#"<div id="language-container-ja" class="language-container"><article id="phrases-ja" class="phrases"><header class="header-pronunciation"><a href="https://forvo.com/languages/ja/">Japanese</a></header><ul id="phrases-list-ja" class="phrases-list"><li class="pronunciation"><div class="play" id="play_8000001">phrase</div></li></ul></article></div>"#,
            true,
        ),
        WORD_MISMATCHED_BUTTON => page(
            &ja_section(
                word,
                &[Row {
                    id: RECORD_A,
                    votes: 5,
                    speaker: "Female from Japan",
                    button_word: word,
                    button_record: RECORD_B,
                    href_record: RECORD_A,
                }],
            ),
            true,
        ),
        WORD_WRONG_HREF => page(
            &ja_section(
                word,
                &[Row {
                    id: RECORD_WRONG_HREF,
                    votes: 5,
                    speaker: "Female from Japan",
                    button_word: word,
                    button_record: RECORD_WRONG_HREF,
                    href_record: RECORD_B,
                }],
            ),
            true,
        ),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_paths_round_trip_through_encoding() {
        assert_eq!(percent_decode(&encode(WORD)), WORD);
        assert_eq!(percent_decode("/word/%E5%85%83%E6%B0%97/"), "/word/元気/");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn fixture_pages_follow_dom_contract_selectors() {
        let html = word_page(WORD).unwrap();
        for needle in [
            "class=\"page_word\"",
            "id=\"language-container-ja\"",
            "id=\"pronunciations-list-ja\"",
            "id=\"play_3661006\"",
            "data-p4=\"3661006\"",
            "/logout/",
        ] {
            assert!(html.contains(needle), "{needle}");
        }
        assert!(!word_page(WORD_LOGIN).unwrap().contains("/logout/"));
        assert!(word_page("нет").is_none());
    }
}
