//! Языковая модель: OpenAI-совместимый `/chat/completions` с потоковым
//! разбором SSE и вызовом инструментов.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use bytes::Bytes;
use futures_util::StreamExt;
use petrovich_core::chat::{ChatMessage, FunctionCall, LlmEvent, ToolCall, ToolDefinition};
use petrovich_core::config::LlmConfig;
use petrovich_core::Secrets;
use reqwest::Response;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::http::{client, endpoint, redact, SseDecoder};

#[derive(Clone)]
pub struct OpenAiLlm {
    http: reqwest::Client,
    url: String,
    models: Vec<String>,
    keys: Vec<String>,
    next_key: Arc<AtomicUsize>,
    temperature: f32,
    max_tokens: u32,
    retries: u8,
    stream: bool,
    timeout: Duration,
}

impl OpenAiLlm {
    pub fn new(cfg: &LlmConfig, secrets: &Secrets) -> Result<Self> {
        let mut models = Vec::with_capacity(1 + cfg.fallback_models.len());
        for model in std::iter::once(&cfg.model).chain(&cfg.fallback_models) {
            let model = model.trim();
            if !model.is_empty() && !models.iter().any(|m| m == model) {
                models.push(model.to_owned());
            }
        }
        if models.is_empty() {
            bail!("LLM: не задана модель");
        }
        let mut keys: Vec<String> = Vec::new();
        for env in std::iter::once(cfg.api_key_env.as_str())
            .chain(cfg.api_key_envs.iter().map(String::as_str))
        {
            let env = env.trim();
            if env.is_empty() {
                continue;
            }
            if let Some(key) = secrets.get(env).filter(|k| !k.is_empty()) {
                if !keys.iter().any(|k| k == key) {
                    keys.push(key.to_owned());
                }
            }
        }
        Ok(Self {
            http: client(cfg.timeout_ms)?,
            url: endpoint(&cfg.base_url, "/chat/completions"),
            models,
            keys,
            next_key: Arc::new(AtomicUsize::new(0)),
            temperature: cfg.temperature,
            max_tokens: cfg.max_tokens,
            retries: cfg.retries,
            stream: cfg.stream,
            timeout: Duration::from_millis(cfg.timeout_ms),
        })
    }

    fn body(&self, model: &str, messages: &[ChatMessage], tools: &[ToolDefinition]) -> Value {
        let mut body = json!({
            "model": model,
            "messages": messages,
            "temperature": self.temperature,
            "max_tokens": self.max_tokens,
            "stream": self.stream,
        });
        if !tools.is_empty() {
            body["tools"] = serde_json::to_value(tools).unwrap_or(Value::Null);
            body["tool_choice"] = json!("auto");
        }
        body
    }

    async fn send(&self, messages: &[ChatMessage], tools: &[ToolDefinition]) -> Result<Response> {
        let mut last: Option<anyhow::Error> = None;
        for (model_idx, model) in self.models.iter().enumerate() {
            let has_next = model_idx + 1 < self.models.len();
            let retries = self.retries.max(self.keys.len().saturating_sub(1) as u8);
            for attempt in 0..=retries {
                let body = self.body(model, messages, tools);
                let mut req = self.http.post(&self.url).json(&body);
                if !self.keys.is_empty() {
                    let key =
                        &self.keys[self.next_key.fetch_add(1, Ordering::Relaxed) % self.keys.len()];
                    req = req.bearer_auth(key);
                }
                let result = tokio::time::timeout(self.timeout, req.send()).await;
                match result {
                    Ok(Ok(resp)) if resp.status().is_success() => {
                        if model_idx > 0 {
                            tracing::info!("LLM: ответ от резервной модели {model}");
                        }
                        return Ok(resp);
                    }
                    Ok(Ok(resp)) => {
                        let status = resp.status();
                        let text = redact(&resp.text().await.unwrap_or_default());
                        let transient = status == reqwest::StatusCode::TOO_MANY_REQUESTS
                            || status.is_server_error();
                        // Смена модели полезна не только при лимите: облачные
                        // шлюзы отвергают вызовы инструментов конкретной
                        // модели, хотя другая модель понимает ту же схему.
                        let switch = status == reqwest::StatusCode::TOO_MANY_REQUESTS
                            || status == reqwest::StatusCode::NOT_FOUND
                            || is_tool_schema_failure(status, &text);
                        let err = anyhow::anyhow!("LLM: {model}: HTTP {status}: {text}");
                        let key_limit = status == reqwest::StatusCode::TOO_MANY_REQUESTS
                            || status == reqwest::StatusCode::UNAUTHORIZED;
                        if key_limit && self.keys.len() > 1 && attempt < retries {
                            tracing::warn!(
                                "LLM: {model}: {}; пробую следующий ключ",
                                short_reason(status, &text)
                            );
                            last = Some(err);
                        } else if switch && has_next {
                            tracing::warn!(
                                "LLM: модель {model}: {}; пробую {}",
                                short_reason(status, &text),
                                self.models[model_idx + 1]
                            );
                            last = Some(err);
                            break;
                        } else if !transient || attempt == retries {
                            if transient && has_next {
                                last = Some(err);
                                break;
                            }
                            return Err(err);
                        } else {
                            last = Some(err);
                        }
                    }
                    Ok(Err(e)) if attempt == retries => {
                        last = Some(anyhow::Error::from(e).context("LLM: запрос"));
                        break;
                    }
                    Ok(Err(e)) => last = Some(anyhow::Error::from(e)),
                    Err(_) if attempt == retries => {
                        last = Some(anyhow::anyhow!(
                            "LLM: {model}: тайм-аут {} мс",
                            self.timeout.as_millis()
                        ));
                        break;
                    }
                    Err(_) => last = Some(anyhow::anyhow!("LLM: {model}: тайм-аут")),
                }
                tracing::warn!("LLM: {model}: повтор {}/{}", attempt + 1, retries);
                tokio::time::sleep(Duration::from_millis(400 * 2u64.pow(attempt as u32))).await;
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("LLM: нет ответа")))
    }

    /// Отправляет запрос и возвращает поток событий. Сетевые ошибки до
    /// получения ответа возвращаются сразу; ошибки посреди потока приходят
    /// событием [`LlmEvent::Error`].
    pub async fn chat_stream(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::Receiver<LlmEvent>> {
        let resp = self.send(&messages, &tools).await?;
        let stream_mode = self.stream;
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(async move {
            if stream_mode {
                parse_sse(resp, tx).await;
            } else {
                parse_full(resp, tx).await;
            }
        });
        Ok(rx)
    }
}

/// Ошибка схемы вызова инструмента: модель сгенерировала неразбираемый
/// `tool_call`. Это свойство конкретной модели, поэтому есть смысл взять
/// следующую из списка.
fn is_tool_schema_failure(status: reqwest::StatusCode, text: &str) -> bool {
    status == reqwest::StatusCode::BAD_REQUEST
        && (text.contains("tool_use_failed") || text.contains("did not match schema"))
}

/// Короткая причина для журнала: полный JSON с `failed_generation` бывает
/// на килобайты и не должен повторяться в каждой строке.
fn short_reason(status: reqwest::StatusCode, text: &str) -> String {
    let message = serde_json::from_str::<Value>(text).ok().and_then(|v| {
        v.pointer("/error/message")
            .or_else(|| v.get("message"))
            .or_else(|| v.get("error"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    match message {
        Some(m) => m.chars().take(180).collect(),
        None => format!(
            "HTTP {status}: {}",
            text.chars().take(180).collect::<String>()
        ),
    }
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

async fn parse_sse(resp: Response, tx: mpsc::Sender<LlmEvent>) {
    let mut sse = SseDecoder::new();
    let mut calls: BTreeMap<i64, PartialCall> = BTreeMap::new();
    let mut finish = None;
    let mut usage = None;
    let mut done = false;
    let mut stream = resp.bytes_stream();
    while let Some(item) = stream.next().await {
        let chunk: Bytes = match item {
            Ok(b) => b,
            Err(e) => {
                let _ = tx
                    .send(LlmEvent::Error(format!("LLM: обрыв потока: {e}")))
                    .await;
                return;
            }
        };
        for data in sse.push(&chunk) {
            if data.trim() == "[DONE]" {
                done = true;
                break;
            }
            let v: Value = match serde_json::from_str(&data) {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx
                        .send(LlmEvent::Error(format!("LLM: неверный JSON: {e}")))
                        .await;
                    return;
                }
            };
            if let Some(err) = v.get("error") {
                let _ = tx
                    .send(LlmEvent::Error(format!(
                        "LLM: {}",
                        redact(&err.to_string())
                    )))
                    .await;
                return;
            }
            if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                usage = Some(u.clone());
            }
            let Some(choice) = v.get("choices").and_then(|c| c.get(0)) else {
                continue;
            };
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                finish = Some(reason.to_owned());
            }
            let Some(delta) = choice.get("delta") else {
                continue;
            };
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                if !text.is_empty() && tx.send(LlmEvent::TextDelta(text.to_owned())).await.is_err()
                {
                    return;
                }
            }
            if let Some(tcs) = delta.get("tool_calls").and_then(Value::as_array) {
                for tc in tcs {
                    let idx = tc.get("index").and_then(Value::as_i64).unwrap_or(0);
                    let slot = calls.entry(idx).or_default();
                    if let Some(id) = tc.get("id").and_then(Value::as_str) {
                        slot.id = id.to_owned();
                    }
                    if let Some(f) = tc.get("function") {
                        if let Some(n) = f.get("name").and_then(Value::as_str) {
                            slot.name.push_str(n);
                        }
                        if let Some(a) = f.get("arguments").and_then(Value::as_str) {
                            slot.arguments.push_str(a);
                        }
                    }
                }
            }
        }
        if done {
            break;
        }
    }
    if !calls.is_empty() {
        let calls: Vec<ToolCall> = calls
            .into_values()
            .filter(|c| !c.name.is_empty())
            .map(|c| ToolCall {
                id: if c.id.is_empty() {
                    format!("call_{}", c.name)
                } else {
                    c.id
                },
                kind: "function".into(),
                function: FunctionCall {
                    name: c.name,
                    arguments: if c.arguments.is_empty() {
                        "{}".into()
                    } else {
                        c.arguments
                    },
                },
            })
            .collect();
        if !calls.is_empty() && tx.send(LlmEvent::ToolCalls(calls)).await.is_err() {
            return;
        }
    }
    let _ = tx
        .send(LlmEvent::Done {
            finish_reason: finish,
            usage,
        })
        .await;
}

async fn parse_full(resp: Response, tx: mpsc::Sender<LlmEvent>) {
    let text = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            let _ = tx.send(LlmEvent::Error(format!("LLM: чтение: {e}"))).await;
            return;
        }
    };
    let v: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            let _ = tx
                .send(LlmEvent::Error(format!(
                    "LLM: не JSON: {e}: {}",
                    redact(&text)
                )))
                .await;
            return;
        }
    };
    if let Some(err) = v.get("error") {
        let _ = tx
            .send(LlmEvent::Error(format!(
                "LLM: {}",
                redact(&err.to_string())
            )))
            .await;
        return;
    }
    let choice = v.get("choices").and_then(|c| c.get(0));
    let message = choice.and_then(|c| c.get("message"));
    if let Some(text) = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
    {
        if !text.is_empty() && tx.send(LlmEvent::TextDelta(text.to_owned())).await.is_err() {
            return;
        }
    }
    if let Some(tcs) = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
    {
        let calls: Vec<ToolCall> = tcs
            .iter()
            .filter_map(|tc| {
                let f = tc.get("function")?;
                Some(ToolCall {
                    id: tc
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("call")
                        .to_owned(),
                    kind: "function".into(),
                    function: FunctionCall {
                        name: f.get("name")?.as_str()?.to_owned(),
                        arguments: f
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}")
                            .to_owned(),
                    },
                })
            })
            .collect();
        if !calls.is_empty() && tx.send(LlmEvent::ToolCalls(calls)).await.is_err() {
            return;
        }
    }
    let _ = tx
        .send(LlmEvent::Done {
            finish_reason: choice
                .and_then(|c| c.get("finish_reason"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            usage: v.get("usage").filter(|u| !u.is_null()).cloned(),
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{serve_once, serve_sequence};

    fn cfg(url: &str, stream: bool) -> LlmConfig {
        LlmConfig {
            base_url: url.into(),
            stream,
            timeout_ms: 5000,
            retries: 0,
            ..LlmConfig::default()
        }
    }

    fn secrets_with(pairs: &[(&str, &str)]) -> Secrets {
        let path = std::env::temp_dir().join(format!(
            "petrovich-secrets-{}-{}.env",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let text: String = pairs.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
        std::fs::write(&path, text).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let secrets = Secrets::load(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        secrets
    }

    async fn collect(mut rx: mpsc::Receiver<LlmEvent>) -> (String, Vec<ToolCall>, Option<String>) {
        let mut text = String::new();
        let mut calls = Vec::new();
        let mut err = None;
        while let Some(ev) = rx.recv().await {
            match ev {
                LlmEvent::TextDelta(t) => text.push_str(&t),
                LlmEvent::ToolCalls(c) => calls = c,
                LlmEvent::Done { .. } => {}
                LlmEvent::Error(e) => err = Some(e),
            }
        }
        (text, calls, err)
    }

    #[tokio::test]
    async fn streams_text() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"При\"}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\"вет\"}}]}\n\n\
                    data: [DONE]\n\n";
        let (url, handle) = serve_once(200, "text/event-stream", body).await;
        let llm = OpenAiLlm::new(&cfg(&url, true), &Secrets::default()).unwrap();
        let rx = llm
            .chat_stream(vec![ChatMessage::user("hi")], vec![])
            .await
            .unwrap();
        let (text, calls, err) = collect(rx).await;
        assert_eq!(text, "Привет");
        assert!(calls.is_empty());
        assert_eq!(err, None);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn accumulates_tool_call_arguments() {
        let body = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"timer\",\"arguments\":\"{\\\"sec\"}}]}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"onds\\\":5}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n\
                    data: [DONE]\n\n";
        let (url, handle) = serve_once(200, "text/event-stream", body).await;
        let llm = OpenAiLlm::new(&cfg(&url, true), &Secrets::default()).unwrap();
        let rx = llm
            .chat_stream(vec![ChatMessage::user("timer")], vec![])
            .await
            .unwrap();
        let (text, calls, err) = collect(rx).await;
        assert!(text.is_empty());
        assert_eq!(err, None);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "c1");
        assert_eq!(calls[0].function.name, "timer");
        assert_eq!(calls[0].function.arguments, "{\"seconds\":5}");
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn parses_non_stream_response() {
        let body = r#"{"choices":[{"message":{"content":"ответ"},"finish_reason":"stop"}],"usage":{"total_tokens":3}}"#;
        let (url, handle) = serve_once(200, "application/json", body).await;
        let llm = OpenAiLlm::new(&cfg(&url, false), &Secrets::default()).unwrap();
        let rx = llm
            .chat_stream(vec![ChatMessage::user("hi")], vec![])
            .await
            .unwrap();
        let (text, calls, err) = collect(rx).await;
        assert_eq!(text, "ответ");
        assert!(calls.is_empty());
        assert_eq!(err, None);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn http_error_is_returned_before_stream() {
        let (url, handle) = serve_once(401, "application/json", r#"{"error":"no key"}"#).await;
        let llm = OpenAiLlm::new(&cfg(&url, true), &Secrets::default()).unwrap();
        let e = llm.chat_stream(vec![ChatMessage::user("hi")], vec![]).await;
        assert!(e.is_err());
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn switches_to_fallback_model_on_rate_limit() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"резерв\"}}]}\n\n\
                   data: [DONE]\n\n";
        let (url, handle) = serve_sequence(vec![
            (
                429,
                "application/json".into(),
                br#"{"error":{"message":"tokens per day","code":"rate_limit_exceeded"}}"#.to_vec(),
            ),
            (200, "text/event-stream".into(), sse.as_bytes().to_vec()),
        ])
        .await;
        let mut config = cfg(&url, true);
        config.model = "primary".into();
        config.fallback_models = vec!["backup".into()];
        let llm = OpenAiLlm::new(&config, &Secrets::default()).unwrap();
        let rx = llm
            .chat_stream(vec![ChatMessage::user("hi")], vec![])
            .await
            .unwrap();
        let (text, calls, err) = collect(rx).await;
        assert_eq!(text, "резерв");
        assert!(calls.is_empty());
        assert_eq!(err, None);
        let requests = handle.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["model"], "primary");
        assert_eq!(requests[1]["model"], "backup");
    }

    #[tokio::test]
    async fn retries_with_next_key_before_model_fallback() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"второй ключ\"}}]}\n\n\
                   data: [DONE]\n\n";
        let (url, handle) = serve_sequence(vec![
            (
                429,
                "application/json".into(),
                br#"{"error":{"message":"tokens per day","code":"rate_limit_exceeded"}}"#.to_vec(),
            ),
            (200, "text/event-stream".into(), sse.as_bytes().to_vec()),
        ])
        .await;
        let mut config = cfg(&url, true);
        config.model = "primary".into();
        config.retries = 0;
        config.api_key_env = "KEY1".into();
        config.api_key_envs = vec!["KEY2".into()];
        let secrets = secrets_with(&[("KEY1", "one"), ("KEY2", "two")]);
        let llm = OpenAiLlm::new(&config, &secrets).unwrap();
        let rx = llm
            .chat_stream(vec![ChatMessage::user("hi")], vec![])
            .await
            .unwrap();
        let (text, calls, err) = collect(rx).await;
        assert_eq!(text, "второй ключ");
        assert!(calls.is_empty());
        assert_eq!(err, None);
        let requests = handle.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["model"], "primary");
        assert_eq!(requests[1]["model"], "primary");
    }

    #[tokio::test]
    async fn switches_to_fallback_on_broken_tool_call() {
        let body = r#"{"choices":[{"message":{"content":"готово"},"finish_reason":"stop"}]}"#;
        let (url, handle) = serve_sequence(vec![
            (
                400,
                "application/json".into(),
                br#"{"error":{"message":"tool call validation failed","code":"tool_use_failed"}}"#
                    .to_vec(),
            ),
            (200, "application/json".into(), body.as_bytes().to_vec()),
        ])
        .await;
        let mut config = cfg(&url, false);
        config.model = "primary".into();
        config.fallback_models = vec!["backup".into()];
        let llm = OpenAiLlm::new(&config, &Secrets::default()).unwrap();
        let rx = llm
            .chat_stream(vec![ChatMessage::user("hi")], vec![])
            .await
            .unwrap();
        let (text, _, err) = collect(rx).await;
        assert_eq!(text, "готово");
        assert_eq!(err, None);
        let requests = handle.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1]["model"], "backup");
    }

    #[tokio::test]
    async fn auth_error_does_not_try_fallbacks() {
        let (url, handle) = serve_sequence(vec![(
            401,
            "application/json".into(),
            br#"{"error":"no key"}"#.to_vec(),
        )])
        .await;
        let mut config = cfg(&url, true);
        config.fallback_models = vec!["backup".into()];
        let llm = OpenAiLlm::new(&config, &Secrets::default()).unwrap();
        let e = llm.chat_stream(vec![ChatMessage::user("hi")], vec![]).await;
        assert!(e.is_err());
        let requests = handle.await.unwrap();
        assert_eq!(requests.len(), 1);
    }
}
