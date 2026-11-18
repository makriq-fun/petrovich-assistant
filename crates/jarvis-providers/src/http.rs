//! Общие HTTP-утилиты провайдеров: TLS с проверкой сертификатов, тайм-ауты,
//! разбор SSE и повторные попытки.

use std::future::Future;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, Response, StatusCode};
use tokio::sync::mpsc;

pub fn client(timeout_ms: u64) -> Result<Client> {
    client_with_ca(timeout_ms, None)
}

/// Клиент с дополнительным корневым сертификатом для self-hosted HTTPS.
pub fn client_with_ca(timeout_ms: u64, ca_file: Option<&std::path::Path>) -> Result<Client> {
    let mut builder = Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .connect_timeout(Duration::from_secs(10))
        .user_agent(concat!("jarvis-station/", env!("CARGO_PKG_VERSION")));
    if let Some(path) = ca_file {
        let pem = std::fs::read(path).with_context(|| format!("сертификат {}", path.display()))?;
        let cert = reqwest::Certificate::from_pem(&pem)
            .with_context(|| format!("разбор сертификата {}", path.display()))?;
        builder = builder.add_root_certificate(cert);
    }
    builder.build().context("создание HTTP-клиента")
}

pub fn endpoint(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

pub fn auth_headers(key: Option<&str>) -> Result<HeaderMap> {
    let mut h = HeaderMap::new();
    h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Some(key) = key.filter(|k| !k.is_empty()) {
        let mut v = HeaderValue::from_str(&format!("Bearer {key}"))
            .context("недопустимые символы в API-ключе")?;
        v.set_sensitive(true);
        h.insert(AUTHORIZATION, v);
    }
    Ok(h)
}

pub async fn check_status(resp: Response, what: &str) -> Result<Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let body = redact(&body);
    bail!("{what}: HTTP {status}: {body}");
}

/// Убирает похожие на ключи строки из тела ошибки.
pub fn redact(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for token in s.split_whitespace() {
        if token.len() >= 24
            && (token.starts_with("sk-")
                || token.starts_with("Bearer")
                || token.chars().filter(|c| c.is_ascii_alphanumeric()).count() == token.len())
        {
            out.push_str("[скрыто]");
        } else {
            out.push_str(token);
        }
        out.push(' ');
    }
    out.trim_end().to_owned()
}

pub fn retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Потоковый разбор SSE: отдаёт содержимое каждого `data:`.
pub struct SseDecoder {
    buf: Vec<u8>,
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl SseDecoder {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = find_separator(&self.buf) {
            let event: Vec<u8> = self.buf.drain(..pos.0 + pos.1).collect();
            let text = String::from_utf8_lossy(&event[..pos.0]);
            let mut data = String::new();
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(rest.trim_start());
                }
            }
            if !data.is_empty() {
                out.push(data);
            }
        }
        out
    }
}

fn find_separator(buf: &[u8]) -> Option<(usize, usize)> {
    for i in 0..buf.len() {
        if buf[i..].starts_with(b"\n\n") {
            return Some((i, 2));
        }
        if buf[i..].starts_with(b"\r\n\r\n") {
            return Some((i, 4));
        }
    }
    None
}

/// Собирает поток байтов в канал; ошибки чтения передаются вызывающему.
pub async fn pump_response(resp: Response, tx: mpsc::Sender<Result<Bytes>>) -> Result<()> {
    let mut stream = resp.bytes_stream();
    while let Some(item) = stream.next().await {
        match item {
            Ok(b) => {
                if tx.send(Ok(b)).await.is_err() {
                    return Ok(());
                }
            }
            Err(e) => {
                let _ = tx.send(Err(anyhow::anyhow!("чтение ответа: {e}"))).await;
                return Err(e).context("чтение ответа");
            }
        }
    }
    Ok(())
}

/// Выполняет запрос с повторами на 429/5xx и сетевые ошибки.
pub async fn with_retries<F, Fut>(what: &str, retries: u8, mut f: F) -> Result<Response>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Response>>,
{
    let mut attempt = 0u8;
    loop {
        match f().await {
            Ok(resp) if resp.status().is_success() => return Ok(resp),
            Ok(resp) if retryable(resp.status()) && attempt < retries => {
                let status = resp.status();
                let body = redact(&resp.text().await.unwrap_or_default());
                tracing::warn!(
                    "{what}: HTTP {status} ({body}); повтор {}/{retries}",
                    attempt + 1
                );
            }
            Ok(resp) => return check_status(resp, what).await,
            Err(e) if attempt < retries => {
                tracing::warn!("{what}: {e:#}; повтор {}/{retries}", attempt + 1);
            }
            Err(e) => return Err(e).with_context(|| format!("{what}: попытки исчерпаны")),
        }
        attempt += 1;
        tokio::time::sleep(Duration::from_millis(300 * 2u64.pow(u32::from(attempt)))).await;
    }
}

/// Читает поток целиком с ограничением размера.
pub async fn read_limited(resp: Response, max_bytes: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(item) = stream.next().await {
        let b = item.context("чтение ответа")?;
        if out.len() + b.len() > max_bytes {
            bail!("ответ превышает {max_bytes} байт");
        }
        out.extend_from_slice(&b);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_handles_split_events() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: {\"a\":1}").is_empty());
        assert_eq!(d.push(b"\n\ndata: [DONE]\n\n"), vec!["{\"a\":1}", "[DONE]"]);
    }

    #[test]
    fn sse_handles_crlf_and_comments() {
        let mut d = SseDecoder::new();
        let out = d.push(b": keepalive\r\n\r\ndata: hi\r\n\r\n");
        assert_eq!(out, ["hi"]);
    }

    #[test]
    fn sse_ignores_empty_data() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"event: ping\n\n").is_empty());
    }

    #[test]
    fn redacts_long_tokens() {
        let s =
            redact("bad key sk-abcdefghijklmnopqrstuvwxyz and Bearer abcdefghijklmnopqrstuvwxyz");
        assert!(!s.contains("abcdefghijklmnopqrstuvwxyz"));
        assert!(s.contains("[скрыто]"));
    }

    #[test]
    fn endpoint_joins_slashes() {
        assert_eq!(endpoint("https://x/v1/", "/chat"), "https://x/v1/chat");
        assert_eq!(endpoint("https://x/v1", "chat"), "https://x/v1/chat");
    }
}
