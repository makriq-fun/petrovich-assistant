//! Общие HTTP-утилиты провайдеров: TLS с проверкой сертификатов, тайм-ауты,
//! разбор SSE и повторные попытки.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, Response, StatusCode};

/// Сколько байт тела ошибки показывать в журнале.
const ERROR_BODY_LIMIT: usize = 64 * 1024;

/// Клиент с корневым сертификатом и SOCKS5/HTTP-прокси.
///
/// Общий `timeout` клиента здесь намеренно не задаётся: в reqwest он
/// ограничивает весь запрос вместе с телом, то есть обрывает потоковые
/// ответы LLM и TTS посреди фразы. Каждый провайдер ограничивает `send()`
/// своим тайм-аутом, а чтение потока — тайм-аутом простоя.
pub fn client_with_ca_proxy(ca_file: Option<&std::path::Path>, proxy: &str) -> Result<Client> {
    let mut builder = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .user_agent(concat!("petrovich-assistant/", env!("CARGO_PKG_VERSION")));
    if let Some(path) = ca_file {
        let pem = std::fs::read(path).with_context(|| format!("сертификат {}", path.display()))?;
        let cert = reqwest::Certificate::from_pem(&pem)
            .with_context(|| format!("разбор сертификата {}", path.display()))?;
        builder = builder.add_root_certificate(cert);
    }
    if !proxy.trim().is_empty() {
        builder = builder.proxy(
            reqwest::Proxy::all(proxy.trim())
                .with_context(|| format!("прокси провайдера {proxy}"))?,
        );
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
    // Тело ошибки может не прийти вовсе: не держим ход из-за него.
    let body =
        match tokio::time::timeout(Duration::from_secs(10), read_prefix(resp, ERROR_BODY_LIMIT))
            .await
        {
            Ok(body) => body,
            Err(_) => "[тело ошибки не прочитано за 10 с]".to_owned(),
        };
    bail!("{what}: HTTP {status}: {body}");
}

/// Читает начало тела ответа, не больше `limit` байт, и вычищает секреты.
/// Тело ошибки не должно ни раздувать память, ни попадать в журнал целиком.
pub async fn read_prefix(resp: Response, limit: usize) -> String {
    let mut out = Vec::new();
    let mut stream = resp.bytes_stream();
    while out.len() < limit {
        let Some(item) = stream.next().await else {
            break;
        };
        let Ok(chunk) = item else {
            break;
        };
        let take = (limit - out.len()).min(chunk.len());
        out.extend_from_slice(&chunk[..take]);
        if take < chunk.len() {
            break;
        }
    }
    redact(&String::from_utf8_lossy(&out))
}

/// Убирает похожие на ключи фрагменты из тела ошибки.
///
/// Ключи ищутся как непрерывные ASCII-последовательности, поэтому строка
/// вида `{"key":"sk-…"}` тоже очищается, а не только токен через пробел.
pub fn redact(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run_start: Option<usize> = None;
    let mut after_bearer = false;
    for (idx, ch) in s.char_indices() {
        let secret_char = ch.is_ascii_alphanumeric() || ch == '-' || ch == '_';
        match (secret_char, run_start) {
            (true, None) => run_start = Some(idx),
            (false, Some(start)) => {
                let run = &s[start..idx];
                push_masked(run, &mut out, after_bearer);
                after_bearer = run.eq_ignore_ascii_case("bearer");
                run_start = None;
                out.push(ch);
            }
            (false, None) => out.push(ch),
            (true, Some(_)) => {}
        }
    }
    if let Some(start) = run_start {
        push_masked(&s[start..], &mut out, after_bearer);
    }
    out
}

fn push_masked(run: &str, out: &mut String, after_bearer: bool) {
    if is_secret_like(run) || (after_bearer && run.len() >= 24) {
        out.push_str("[скрыто]");
    } else {
        out.push_str(run);
    }
}

fn is_secret_like(run: &str) -> bool {
    if run.len() < 24 {
        return false;
    }
    if run.starts_with("sk-") || run.starts_with("pk-") {
        return true;
    }
    let mut letters = false;
    let mut digits = false;
    for ch in run.chars() {
        if !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_') {
            return false;
        }
        letters |= ch.is_ascii_alphabetic();
        digits |= ch.is_ascii_digit();
    }
    letters && digits
}

pub fn retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Задержка перед повтором: экспонента с потолком и разбросом, чтобы
/// несколько колонок не били в провайдер синхронно после сбоя.
pub fn backoff(attempt: u8) -> Duration {
    let base_ms = 300u64.saturating_mul(1u64 << u32::from(attempt).min(5));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0) as u64;
    let jitter = base_ms * (nanos % 100) / 400;
    Duration::from_millis(base_ms + jitter)
}

/// `Retry-After` в секундах, разумно ограниченный сверху.
pub fn retry_after(resp: &Response) -> Option<Duration> {
    let value = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    value
        .parse::<u64>()
        .ok()
        .map(|s| Duration::from_secs(s.min(30)))
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
        let mut start = 0;
        // `start` только растёт, поэтому буфер не пересканируется целиком
        // для каждого события: на длинном ответе это O(n), а не O(n²).
        while let Some((pos, sep)) = find_separator(&self.buf[start..]) {
            let end = start + pos;
            let text = String::from_utf8_lossy(&self.buf[start..end]);
            let mut data = String::new();
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("data:") {
                    // По спецификации после `data:` допускается ровно один
                    // пробел; остальные пробелы — часть данных.
                    let rest = rest.strip_prefix(' ').unwrap_or(rest);
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(rest);
                }
            }
            if !data.is_empty() {
                out.push(data);
            }
            start = end + sep;
        }
        if start > 0 {
            self.buf.drain(..start);
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
    fn sse_keeps_second_space() {
        let mut d = SseDecoder::new();
        assert_eq!(d.push(b"data:  x\n\n"), [" x"]);
    }

    #[test]
    fn sse_many_events_in_one_chunk() {
        let mut d = SseDecoder::new();
        let mut chunk = Vec::new();
        for i in 0..500 {
            chunk.extend_from_slice(format!("data: {i}\n\n").as_bytes());
        }
        let out = d.push(&chunk);
        assert_eq!(out.len(), 500);
        assert_eq!(out[0], "0");
        assert_eq!(out[499], "499");
        assert!(d.buf.is_empty());
    }

    #[test]
    fn redacts_long_tokens() {
        let s =
            redact("bad key sk-abcdefghijklmnopqrstuvwxyz and Bearer abcdefghijklmnopqrstuvwxyz");
        assert!(!s.contains("abcdefghijklmnopqrstuvwxyz"));
        assert!(s.contains("[скрыто]"));
    }

    #[test]
    fn redacts_quoted_secret() {
        let s = redact(r#"{"api_key":"a1b2c3d4e5f6g7h8i9j0k1l2m3n4"}"#);
        assert!(!s.contains("a1b2c3d4e5f6g7h8i9j0k1l2m3n4"), "{s}");
        assert!(s.contains("[скрыто]"));
    }

    #[test]
    fn keeps_short_words_and_urls() {
        let s = redact("см. https://example.com/path?q=1 и слово");
        assert_eq!(s, "см. https://example.com/path?q=1 и слово");
    }

    #[test]
    fn backoff_is_bounded() {
        assert!(backoff(0) >= Duration::from_millis(300));
        assert!(backoff(10) <= Duration::from_millis(300 * 32 + 300 * 32 / 4));
    }

    #[test]
    fn endpoint_joins_slashes() {
        assert_eq!(endpoint("https://x/v1/", "/chat"), "https://x/v1/chat");
        assert_eq!(endpoint("https://x/v1", "chat"), "https://x/v1/chat");
    }
}
