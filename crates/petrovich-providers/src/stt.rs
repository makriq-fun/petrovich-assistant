//! Распознавание речи: OpenAI-совместимый HTTP (Whisper) или локальная
//! команда. Аудио всегда WAV PCM16; язык задаётся конфигурацией.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use petrovich_core::config::{SttConfig, SttProvider};
use petrovich_core::Secrets;
use reqwest::multipart::{Form, Part};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::http::{check_status, client, client_with_ca, endpoint, redact, retryable};

#[async_trait]
pub trait Stt: Send + Sync {
    async fn transcribe(&self, wav: Vec<u8>) -> Result<String>;
}

pub fn from_config(cfg: &SttConfig, secrets: &Secrets) -> Result<Box<dyn Stt>> {
    match cfg.provider {
        SttProvider::Openai => Ok(Box::new(OpenAiStt::new(cfg, secrets)?)),
        SttProvider::Fish => Ok(Box::new(FishStt::new(cfg, secrets)?)),
        SttProvider::Command => Ok(Box::new(CommandStt::new(
            cfg.command.clone(),
            cfg.timeout_ms,
        )?)),
    }
}

pub struct FishStt {
    http: reqwest::Client,
    url: String,
    model: String,
    language: String,
    key: Option<String>,
    retries: u8,
}

impl FishStt {
    pub fn new(cfg: &SttConfig, secrets: &Secrets) -> Result<Self> {
        Ok(Self {
            http: client_with_ca(cfg.timeout_ms, cfg.tls_ca_file.as_deref())?,
            url: endpoint(&cfg.base_url, "/asr"),
            model: cfg.model.clone(),
            language: cfg.language.clone(),
            key: secrets
                .get(&cfg.api_key_env)
                .filter(|k| !k.is_empty())
                .map(str::to_owned),
            retries: cfg.retries,
        })
    }
}

#[async_trait]
impl Stt for FishStt {
    async fn transcribe(&self, wav: Vec<u8>) -> Result<String> {
        let mut attempt = 0u8;
        loop {
            let part = Part::bytes(wav.clone())
                .file_name("audio.wav")
                .mime_str("audio/wav")
                .context("multipart WAV")?;
            let mut form = Form::new()
                .part("audio", part)
                .text("ignore_timestamps", "true")
                .text("tag_audio_events", "false")
                .text("diarize", "false");
            if !self.language.is_empty() {
                form = form.text("language", self.language.clone());
            }
            let mut req = self
                .http
                .post(&self.url)
                .header("model", &self.model)
                .multipart(form);
            if let Some(key) = &self.key {
                req = req.bearer_auth(key);
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => {
                    let text = resp.text().await.context("Fish STT: чтение")?;
                    let v: serde_json::Value = serde_json::from_str(&text)
                        .with_context(|| format!("Fish STT: не JSON: {}", redact(&text)))?;
                    let raw = v
                        .get("text")
                        .and_then(|t| t.as_str())
                        .ok_or_else(|| anyhow::anyhow!("Fish STT: нет text"))?;
                    let cleaned = strip_speaker_markers(raw);
                    if cleaned.is_empty() {
                        bail!("Fish STT: пустой текст");
                    }
                    return Ok(cleaned);
                }
                Ok(resp) if retryable(resp.status()) && attempt < self.retries => {
                    tracing::warn!("Fish STT: HTTP {}; повтор {}", resp.status(), attempt + 1);
                }
                Ok(resp) => return check_status(resp, "Fish STT").await.map(|_| String::new()),
                Err(e) if attempt < self.retries => {
                    tracing::warn!("Fish STT: {e:#}; повтор {}", attempt + 1);
                }
                Err(e) => return Err(e).context("Fish STT: попытки исчерпаны"),
            }
            attempt += 1;
            tokio::time::sleep(Duration::from_millis(400 * 2u64.pow(u32::from(attempt)))).await;
        }
    }
}

fn strip_speaker_markers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<|speaker:") {
        out.push_str(&rest[..start]);
        if let Some(end) = rest[start..].find("|>") {
            rest = &rest[start + end + 2..];
        } else {
            rest = "";
        }
    }
    out.push_str(rest);
    out.trim().to_owned()
}

pub struct OpenAiStt {
    http: reqwest::Client,
    url: String,
    model: String,
    language: String,
    key: Option<String>,
    retries: u8,
}

impl OpenAiStt {
    pub fn new(cfg: &SttConfig, secrets: &Secrets) -> Result<Self> {
        Ok(Self {
            http: client(cfg.timeout_ms)?,
            url: endpoint(&cfg.base_url, "/audio/transcriptions"),
            model: cfg.model.clone(),
            language: cfg.language.clone(),
            key: secrets
                .get(&cfg.api_key_env)
                .filter(|k| !k.is_empty())
                .map(str::to_owned),
            retries: cfg.retries,
        })
    }
}

#[async_trait]
impl Stt for OpenAiStt {
    async fn transcribe(&self, wav: Vec<u8>) -> Result<String> {
        let mut attempt = 0u8;
        loop {
            let part = Part::bytes(wav.clone())
                .file_name("audio.wav")
                .mime_str("audio/wav")
                .context("multipart WAV")?;
            let mut form = Form::new()
                .part("file", part)
                .text("model", self.model.clone())
                .text("response_format", "json")
                .text("temperature", "0");
            if !self.language.is_empty() {
                form = form.text("language", self.language.clone());
            }
            let mut req = self.http.post(&self.url).multipart(form);
            if let Some(key) = &self.key {
                req = req.bearer_auth(key);
            }
            let result = req.send().await;
            match result {
                Ok(resp) if resp.status().is_success() => {
                    let status = resp.status();
                    let resp = check_status(resp, "STT").await?;
                    let text = resp.text().await.context("STT: чтение ответа")?;
                    let value: serde_json::Value =
                        serde_json::from_str(&text).with_context(|| {
                            format!("STT HTTP {status}: не JSON: {}", redact(&text))
                        })?;
                    let text = value
                        .get("text")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| anyhow::anyhow!("STT: в ответе нет text"))?
                        .trim()
                        .to_owned();
                    if text.is_empty() {
                        bail!("STT: пустой текст");
                    }
                    return Ok(text);
                }
                Ok(resp) if retryable(resp.status()) && attempt < self.retries => {
                    let status = resp.status();
                    tracing::warn!("STT: HTTP {status}; повтор {}/{attempt}", attempt + 1);
                }
                Ok(resp) => return check_status(resp, "STT").await.map(|_| String::new()),
                Err(e) if attempt < self.retries => {
                    tracing::warn!("STT: {e:#}; повтор {}", attempt + 1);
                }
                Err(e) => return Err(e).context("STT: попытки исчерпаны"),
            }
            attempt += 1;
            tokio::time::sleep(Duration::from_millis(400 * 2u64.pow(u32::from(attempt)))).await;
        }
    }
}

/// Локальный распознаватель: WAV через stdin или `{file}` в аргументах.
pub struct CommandStt {
    argv: Vec<String>,
    timeout_ms: u64,
}

impl CommandStt {
    pub fn new(argv: Vec<String>, timeout_ms: u64) -> Result<Self> {
        if argv.is_empty() {
            bail!("STT: пустая команда");
        }
        Ok(Self { argv, timeout_ms })
    }
}

#[async_trait]
impl Stt for CommandStt {
    async fn transcribe(&self, wav: Vec<u8>) -> Result<String> {
        let needs_file = self.argv.iter().any(|a| a.contains("{file}"));
        let mut temp: Option<PathBuf> = None;
        let mut argv = self.argv.clone();
        if needs_file {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir()
                .join(format!("petrovich-stt-{}-{stamp}.wav", std::process::id()));
            tokio::fs::write(&path, &wav)
                .await
                .with_context(|| format!("запись {}", path.display()))?;
            for a in &mut argv {
                *a = a.replace("{file}", &path.to_string_lossy());
            }
            temp = Some(path);
        }
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(if needs_file {
                Stdio::null()
            } else {
                Stdio::piped()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("запуск {}", argv[0]))?;
        if !needs_file {
            let mut stdin = child.stdin.take().context("STT: нет stdin")?;
            let wav2 = wav.clone();
            tokio::spawn(async move {
                let _ = stdin.write_all(&wav2).await;
            });
        }
        let fut = child.wait_with_output();
        let out = match tokio::time::timeout(Duration::from_millis(self.timeout_ms), fut).await {
            Ok(r) => r.context("STT: ожидание процесса")?,
            Err(_) => {
                if let Some(p) = temp {
                    let _ = tokio::fs::remove_file(p).await;
                }
                bail!("STT: тайм-аут {} мс", self.timeout_ms);
            }
        };
        if let Some(p) = temp {
            let _ = tokio::fs::remove_file(p).await;
        }
        if !out.status.success() {
            bail!(
                "STT: команда завершилась с {}: {}",
                out.status,
                redact(&String::from_utf8_lossy(&out.stderr))
            );
        }
        let raw = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        let text = if raw.starts_with('{') {
            serde_json::from_str::<serde_json::Value>(&raw)
                .ok()
                .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(str::to_owned))
                .unwrap_or(raw)
        } else {
            raw
        };
        if text.trim().is_empty() {
            bail!("STT: пустой текст");
        }
        Ok(text.trim().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::serve_once;

    #[tokio::test]
    async fn command_stt_reads_stdin() {
        let s = CommandStt::new(
            vec![
                "sh".into(),
                "-c".into(),
                "cat >/dev/null; echo привет".into(),
            ],
            5000,
        )
        .unwrap();
        assert_eq!(s.transcribe(vec![0; 100]).await.unwrap(), "привет");
    }

    #[tokio::test]
    async fn command_stt_uses_file_placeholder() {
        let s = CommandStt::new(
            vec![
                "sh".into(),
                "-c".into(),
                "test -s \"$1\" && echo файл".into(),
                "sh".into(),
                "{file}".into(),
            ],
            5000,
        )
        .unwrap();
        assert_eq!(s.transcribe(vec![0; 100]).await.unwrap(), "файл");
    }

    #[tokio::test]
    async fn fish_stt_parses_text() {
        let (url, handle) = serve_once(200, "application/json", r#"{"text":" Петрович? "}"#).await;
        let cfg = SttConfig {
            provider: SttProvider::Fish,
            base_url: url,
            model: "transcribe-1".into(),
            timeout_ms: 5000,
            ..SttConfig::default()
        };
        let s = FishStt::new(&cfg, &Secrets::default()).unwrap();
        assert_eq!(s.transcribe(vec![0; 10]).await.unwrap(), "Петрович?");
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn openai_stt_parses_json() {
        let (url, handle) =
            serve_once(200, "application/json", r#"{"text":" включи свет "}"#).await;
        let cfg = SttConfig {
            base_url: url,
            timeout_ms: 5000,
            ..SttConfig::default()
        };
        let s = OpenAiStt::new(&cfg, &Secrets::default()).unwrap();
        assert_eq!(s.transcribe(vec![0; 10]).await.unwrap(), "включи свет");
        handle.await.unwrap();
    }
}
