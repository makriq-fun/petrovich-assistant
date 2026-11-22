//! Синтез речи: OpenAI-совместимый `/audio/speech` (PCM или WAV) либо
//! локальная команда. PCM отдаётся потоком; WAV буферизуется и проверяется.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream::{BoxStream, StreamExt};
use petrovich_core::config::{TtsConfig, TtsFormat, TtsProvider};
use petrovich_core::Secrets;
use reqwest::Response;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::http::{
    backoff, client_with_ca_proxy, endpoint, read_limited, read_prefix, redact, retry_after,
};

pub struct AudioStream {
    pub format: TtsFormat,
    pub sample_rate: u32,
    pub channels: u16,
    pub stream: BoxStream<'static, Result<Bytes>>,
}

/// Состояние потоковой проверки ответа TTS.
struct GuardedStream {
    stream: BoxStream<'static, Result<Bytes>>,
    timeout: Duration,
    max_bytes: usize,
    left: usize,
    first: bool,
    failed: bool,
    expect_pcm: bool,
}

/// Оборачивает поток TTS проверками, которых не делает HTTP-клиент:
/// тайм-аут простоя (сервер замолчал), лимит размера (защита RAM) и
/// отказ от WAV-заголовка там, где обещан сырой PCM.
fn guarded_stream(
    resp: Response,
    timeout: Duration,
    max_bytes: usize,
    expect_pcm: bool,
) -> BoxStream<'static, Result<Bytes>> {
    let state = GuardedStream {
        stream: resp
            .bytes_stream()
            .map(|r| r.map_err(anyhow::Error::from))
            .boxed(),
        timeout,
        max_bytes,
        left: max_bytes,
        first: true,
        failed: false,
        expect_pcm,
    };
    futures_util::stream::unfold(state, |mut st| async move {
        if st.failed {
            return None;
        }
        match tokio::time::timeout(st.timeout, st.stream.next()).await {
            Ok(None) => None,
            Ok(Some(Err(e))) => {
                st.failed = true;
                Some((Err(anyhow::anyhow!("TTS: чтение потока: {e}")), st))
            }
            Ok(Some(Ok(chunk))) => {
                if st.first {
                    st.first = false;
                    if st.expect_pcm && chunk.starts_with(b"RIFF") {
                        st.failed = true;
                        return Some((
                            Err(anyhow::anyhow!(
                                "TTS: сервер вернул WAV вместо PCM (проверьте tts.format)"
                            )),
                            st,
                        ));
                    }
                }
                if chunk.len() > st.left {
                    st.failed = true;
                    return Some((
                        Err(anyhow::anyhow!(
                            "TTS: ответ больше {} байт (tts.max_bytes)",
                            st.max_bytes
                        )),
                        st,
                    ));
                }
                st.left -= chunk.len();
                Some((Ok(chunk), st))
            }
            Err(_) => {
                st.failed = true;
                Some((
                    Err(anyhow::anyhow!(
                        "TTS: поток молчит дольше {} мс",
                        st.timeout.as_millis()
                    )),
                    st,
                ))
            }
        }
    })
    .boxed()
}

#[async_trait]
pub trait Tts: Send + Sync {
    async fn synthesize(&self, text: &str) -> Result<AudioStream>;
    fn audio_format(&self) -> TtsFormat;
    fn sample_rate(&self) -> u32;
}

pub fn from_config(cfg: &TtsConfig, secrets: &Secrets) -> Result<Box<dyn Tts>> {
    match cfg.provider {
        TtsProvider::Openai => Ok(Box::new(OpenAiTts::new(cfg, secrets)?)),
        TtsProvider::Fish => Ok(Box::new(FishTts::new(cfg, secrets)?)),
        TtsProvider::Command => Ok(Box::new(CommandTts::new(cfg)?)),
    }
}

pub struct FishTts {
    http: reqwest::Client,
    url: String,
    model: String,
    reference_id: String,
    key: Option<String>,
    sample_rate: u32,
    timeout: Duration,
    retries: u8,
    max_bytes: usize,
}

impl FishTts {
    pub fn new(cfg: &TtsConfig, secrets: &Secrets) -> Result<Self> {
        if cfg.voice.trim().is_empty() {
            bail!("Fish TTS: пустой reference_id (tts.voice)");
        }
        Ok(Self {
            http: client_with_ca_proxy(cfg.tls_ca_file.as_deref(), &cfg.proxy)?,
            url: endpoint(&cfg.base_url, "/tts"),
            model: cfg.model.clone(),
            reference_id: cfg.voice.clone(),
            key: secrets
                .get(&cfg.api_key_env)
                .filter(|k| !k.is_empty())
                .map(str::to_owned),
            sample_rate: cfg.sample_rate,
            timeout: Duration::from_millis(cfg.timeout_ms),
            retries: cfg.retries,
            max_bytes: cfg.max_bytes,
        })
    }

    async fn request(&self, text: &str) -> Result<Response> {
        let body = json!({
            "text": text,
            "reference_id": self.reference_id,
            "format": "pcm",
            "sample_rate": self.sample_rate,
            "normalize": true,
        });
        let mut last: Option<anyhow::Error> = None;
        for attempt in 0..=self.retries {
            let mut retry_hint = None;
            let mut req = self
                .http
                .post(&self.url)
                .header("model", &self.model)
                .json(&body);
            if let Some(key) = &self.key {
                req = req.bearer_auth(key);
            }
            match tokio::time::timeout(self.timeout, req.send()).await {
                Ok(Ok(resp)) if resp.status().is_success() => return Ok(resp),
                Ok(Ok(resp)) => {
                    let status = resp.status();
                    retry_hint = retry_after(&resp);
                    let text = read_prefix(resp, 16 * 1024).await;
                    let err = anyhow::anyhow!("Fish TTS: HTTP {status}: {text}");
                    if !(status == reqwest::StatusCode::TOO_MANY_REQUESTS
                        || status.is_server_error())
                        || attempt == self.retries
                    {
                        return Err(err);
                    }
                    last = Some(err);
                }
                Ok(Err(e)) if attempt == self.retries => return Err(e).context("Fish TTS: запрос"),
                Ok(Err(e)) => last = Some(anyhow::Error::from(e)),
                Err(_) if attempt == self.retries => {
                    bail!("Fish TTS: тайм-аут {} мс", self.timeout.as_millis())
                }
                Err(_) => last = Some(anyhow::anyhow!("Fish TTS: тайм-аут")),
            }
            tracing::warn!("Fish TTS: повтор {}/{}", attempt + 1, self.retries);
            tokio::time::sleep(retry_hint.unwrap_or_else(|| backoff(attempt))).await;
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("Fish TTS: нет ответа")))
    }
}

#[async_trait]
impl Tts for FishTts {
    async fn synthesize(&self, text: &str) -> Result<AudioStream> {
        if text.trim().is_empty() {
            bail!("Fish TTS: пустой текст");
        }
        let resp = self.request(text).await?;
        let stream = guarded_stream(resp, self.timeout, self.max_bytes, true);
        Ok(AudioStream {
            format: TtsFormat::Pcm,
            sample_rate: self.sample_rate,
            channels: 1,
            stream,
        })
    }

    fn audio_format(&self) -> TtsFormat {
        TtsFormat::Pcm
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

pub struct OpenAiTts {
    http: reqwest::Client,
    url: String,
    model: String,
    voice: String,
    key: Option<String>,
    format: TtsFormat,
    sample_rate: u32,
    timeout: Duration,
    retries: u8,
    max_bytes: usize,
}

impl OpenAiTts {
    pub fn new(cfg: &TtsConfig, secrets: &Secrets) -> Result<Self> {
        Ok(Self {
            http: client_with_ca_proxy(cfg.tls_ca_file.as_deref(), &cfg.proxy)?,
            url: endpoint(&cfg.base_url, "/audio/speech"),
            model: cfg.model.clone(),
            voice: cfg.voice.clone(),
            key: secrets
                .get(&cfg.api_key_env)
                .filter(|k| !k.is_empty())
                .map(str::to_owned),
            format: cfg.format,
            sample_rate: cfg.sample_rate,
            timeout: Duration::from_millis(cfg.timeout_ms),
            retries: cfg.retries,
            max_bytes: cfg.max_bytes,
        })
    }

    async fn request(&self, text: &str) -> Result<Response> {
        let body = json!({
            "model": self.model,
            "voice": self.voice,
            "input": text,
            "response_format": match self.format { TtsFormat::Pcm => "pcm", TtsFormat::Wav => "wav" },
        });
        let mut last: Option<anyhow::Error> = None;
        for attempt in 0..=self.retries {
            let mut retry_hint = None;
            let mut req = self.http.post(&self.url).json(&body);
            if let Some(key) = &self.key {
                req = req.bearer_auth(key);
            }
            match tokio::time::timeout(self.timeout, req.send()).await {
                Ok(Ok(resp)) if resp.status().is_success() => return Ok(resp),
                Ok(Ok(resp)) => {
                    let status = resp.status();
                    retry_hint = retry_after(&resp);
                    let text = read_prefix(resp, 16 * 1024).await;
                    let err = anyhow::anyhow!("TTS: HTTP {status}: {text}");
                    if !(status == reqwest::StatusCode::TOO_MANY_REQUESTS
                        || status.is_server_error())
                        || attempt == self.retries
                    {
                        return Err(err);
                    }
                    last = Some(err);
                }
                Ok(Err(e)) if attempt == self.retries => return Err(e).context("TTS: запрос"),
                Ok(Err(e)) => last = Some(anyhow::Error::from(e)),
                Err(_) if attempt == self.retries => {
                    bail!("TTS: тайм-аут {} мс", self.timeout.as_millis())
                }
                Err(_) => last = Some(anyhow::anyhow!("TTS: тайм-аут")),
            }
            tracing::warn!("TTS: повтор {}/{}", attempt + 1, self.retries);
            tokio::time::sleep(retry_hint.unwrap_or_else(|| backoff(attempt))).await;
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("TTS: нет ответа")))
    }
}

#[async_trait]
impl Tts for OpenAiTts {
    async fn synthesize(&self, text: &str) -> Result<AudioStream> {
        if text.trim().is_empty() {
            bail!("TTS: пустой текст");
        }
        let resp = self.request(text).await?;
        match self.format {
            TtsFormat::Pcm => {
                let stream = guarded_stream(resp, self.timeout, self.max_bytes, true);
                Ok(AudioStream {
                    format: TtsFormat::Pcm,
                    sample_rate: self.sample_rate,
                    channels: 1,
                    stream,
                })
            }
            TtsFormat::Wav => {
                let bytes = tokio::time::timeout(self.timeout, read_limited(resp, self.max_bytes))
                    .await
                    .context("TTS: тайм-аут ответа")?
                    .context("TTS: чтение ответа")?;
                let (rate, channels, offset) = petrovich_core::wav::probe(&bytes)?;
                let body = Bytes::copy_from_slice(&bytes[offset..]);
                Ok(AudioStream {
                    format: TtsFormat::Wav,
                    sample_rate: rate,
                    channels,
                    stream: futures_util::stream::once(async move { Ok(body) }).boxed(),
                })
            }
        }
    }

    fn audio_format(&self) -> TtsFormat {
        self.format
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

/// Локальный TTS: текст на stdin, PCM/WAV на stdout.
pub struct CommandTts {
    argv: Vec<String>,
    format: TtsFormat,
    sample_rate: u32,
    timeout: Duration,
    max_bytes: usize,
}

impl CommandTts {
    pub fn new(cfg: &TtsConfig) -> Result<Self> {
        if cfg.command.is_empty() {
            bail!("TTS: пустая команда");
        }
        Ok(Self {
            argv: cfg.command.clone(),
            format: cfg.format,
            sample_rate: cfg.sample_rate,
            timeout: Duration::from_millis(cfg.timeout_ms),
            max_bytes: cfg.max_bytes,
        })
    }

    fn spawn(&self, text: &str) -> Result<tokio::process::Child> {
        let mut child = Command::new(&self.argv[0])
            .args(&self.argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("запуск {}", self.argv[0]))?;
        let mut stdin = child.stdin.take().context("TTS: нет stdin")?;
        let text = text.to_owned();
        tokio::spawn(async move {
            let _ = stdin.write_all(text.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });
        Ok(child)
    }
}

#[async_trait]
impl Tts for CommandTts {
    async fn synthesize(&self, text: &str) -> Result<AudioStream> {
        if text.trim().is_empty() {
            bail!("TTS: пустой текст");
        }
        let mut child = self.spawn(text)?;
        match self.format {
            TtsFormat::Pcm => {
                let mut stdout = child.stdout.take().context("TTS: нет stdout")?;
                let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes>>(8);
                let timeout = self.timeout;
                let max_bytes = self.max_bytes;
                // Дочерний процесс живёт в задаче чтения: kill_on_drop не
                // оборвёт поток, пока он нужен.
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt as _;
                    let mut buf = vec![0u8; 8192];
                    let mut total = 0usize;
                    loop {
                        let read = tokio::time::timeout(timeout, stdout.read(&mut buf)).await;
                        match read {
                            Ok(Ok(0)) => break,
                            Ok(Ok(n)) => {
                                total = total.saturating_add(n);
                                if total > max_bytes {
                                    let _ = tx
                                        .send(Err(anyhow::anyhow!(
                                            "TTS: ответ больше {max_bytes} байт"
                                        )))
                                        .await;
                                    let _ = child.kill().await;
                                    break;
                                }
                                if tx
                                    .send(Ok(Bytes::copy_from_slice(&buf[..n])))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Ok(Err(e)) => {
                                let _ = tx.send(Err(anyhow::Error::from(e))).await;
                                break;
                            }
                            Err(_) => {
                                let _ = tx
                                    .send(Err(anyhow::anyhow!(
                                        "TTS: команда молчит дольше {} мс",
                                        timeout.as_millis()
                                    )))
                                    .await;
                                let _ = child.kill().await;
                                break;
                            }
                        }
                    }
                    let _ = child.wait().await;
                });
                let stream = futures_util::stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|item| (item, rx))
                })
                .boxed();
                Ok(AudioStream {
                    format: TtsFormat::Pcm,
                    sample_rate: self.sample_rate,
                    channels: 1,
                    stream,
                })
            }
            TtsFormat::Wav => {
                let out = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
                    Ok(r) => r.context("TTS: ожидание процесса")?,
                    Err(_) => bail!("TTS: тайм-аут {} мс", self.timeout.as_millis()),
                };
                if !out.status.success() {
                    bail!(
                        "TTS: команда вернула {}: {}",
                        out.status,
                        redact(&String::from_utf8_lossy(&out.stderr))
                    );
                }
                if out.stdout.len() > self.max_bytes {
                    bail!("TTS: ответ больше {} байт", self.max_bytes);
                }
                let (rate, channels, offset) = petrovich_core::wav::probe(&out.stdout)?;
                let body = Bytes::from(out.stdout[offset..].to_vec());
                Ok(AudioStream {
                    format: TtsFormat::Wav,
                    sample_rate: rate,
                    channels,
                    stream: futures_util::stream::once(async move { Ok(body) }).boxed(),
                })
            }
        }
    }

    fn audio_format(&self) -> TtsFormat {
        self.format
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::serve_once;

    async fn collect(mut s: AudioStream) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(chunk) = s.stream.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        out
    }

    fn cfg(url: &str, format: TtsFormat) -> TtsConfig {
        TtsConfig {
            base_url: url.into(),
            format,
            timeout_ms: 5000,
            retries: 0,
            ..TtsConfig::default()
        }
    }

    #[tokio::test]
    async fn fish_tts_streams_pcm() {
        let (url, handle) = serve_once(200, "audio/pcm", "FISH").await;
        let cfg = TtsConfig {
            provider: TtsProvider::Fish,
            base_url: url,
            model: "s2.1-pro-free".into(),
            voice: "ref".into(),
            timeout_ms: 5000,
            retries: 0,
            ..TtsConfig::default()
        };
        let t = FishTts::new(&cfg, &Secrets::default()).unwrap();
        assert_eq!(
            collect(t.synthesize("привет").await.unwrap()).await,
            b"FISH"
        );
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn openai_pcm_streams_bytes() {
        let (url, handle) = serve_once(200, "audio/pcm", "PCMDATA").await;
        let t = OpenAiTts::new(&cfg(&url, TtsFormat::Pcm), &Secrets::default()).unwrap();
        let s = t.synthesize("привет").await.unwrap();
        assert_eq!(s.sample_rate, 24_000);
        assert_eq!(collect(s).await, b"PCMDATA");
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn openai_wav_strips_header() {
        let wav = petrovich_core::wav::encode_mono(16_000, &[1, 2, 3, 4]);
        let (url, handle) = crate::testutil::serve_once_bytes(200, "audio/wav", wav.clone()).await;
        let t = OpenAiTts::new(&cfg(&url, TtsFormat::Wav), &Secrets::default()).unwrap();
        let s = t.synthesize("привет").await.unwrap();
        assert_eq!(s.sample_rate, 16_000);
        let body = collect(s).await;
        assert_eq!(body, &wav[44..]);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn openai_pcm_rejects_wav_header() {
        let (url, handle) = serve_once(200, "audio/pcm", "RIFFxxxxWAVE").await;
        let t = OpenAiTts::new(&cfg(&url, TtsFormat::Pcm), &Secrets::default()).unwrap();
        let mut s = t.synthesize("привет").await.unwrap();
        let err = s.stream.next().await.unwrap().unwrap_err().to_string();
        assert!(err.contains("WAV вместо PCM"), "{err}");
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn openai_pcm_respects_max_bytes() {
        let (url, handle) = serve_once(200, "audio/pcm", "0123456789").await;
        let mut c = cfg(&url, TtsFormat::Pcm);
        c.max_bytes = 4;
        let t = OpenAiTts::new(&c, &Secrets::default()).unwrap();
        let mut s = t.synthesize("привет").await.unwrap();
        let mut err = None;
        while let Some(item) = s.stream.next().await {
            if let Err(e) = item {
                err = Some(e.to_string());
                break;
            }
        }
        let err = err.expect("должна быть ошибка лимита");
        assert!(err.contains("больше 4 байт"), "{err}");
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn command_pcm_reads_stdout() {
        let c = TtsConfig {
            provider: TtsProvider::Command,
            command: vec!["sh".into(), "-c".into(), "cat >/dev/null; printf OK".into()],
            format: TtsFormat::Pcm,
            ..TtsConfig::default()
        };
        let t = CommandTts::new(&c).unwrap();
        assert_eq!(collect(t.synthesize("привет").await.unwrap()).await, b"OK");
    }
}
