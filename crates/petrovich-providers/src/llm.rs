//! Языковая модель: OpenAI-совместимый `/chat/completions` с потоковым
//! разбором SSE и вызовом инструментов.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
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
    model: String,
    key: Option<String>,
    temperature: f32,
    max_tokens: u32,
    retries: u8,
    stream: bool,
    timeout: Duration,
}

impl OpenAiLlm {
    pub fn new(cfg: &LlmConfig, secrets: &Secrets) -> Result<Self> {
        Ok(Self {
            http: client(cfg.timeout_ms)?,
            url: endpoint(&cfg.base_url, "/chat/completions"),
            model: cfg.model.clone(),
            key: secrets
                .get(&cfg.api_key_env)
                .filter(|k| !k.is_empty())
                .map(str::to_owned),
            temperature: cfg.temperature,
            max_tokens: cfg.max_tokens,
            retries: cfg.retries,
            stream: cfg.stream,
            timeout: Duration::from_millis(cfg.timeout_ms),
        })
    }

    fn body(&self, messages: &[ChatMessage], tools: &[ToolDefinition]) -> Value {
        let mut body = json!({
            "model": self.model,
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

    async fn send(&self, body: Value) -> Result<Response> {
        let mut last: Option<anyhow::Error> = None;
        for attempt in 0..=self.retries {
            let mut req = self.http.post(&self.url).json(&body);
            if let Some(key) = &self.key {
                req = req.bearer_auth(key);
            }
            let result = tokio::time::timeout(self.timeout, req.send()).await;
            match result {
                Ok(Ok(resp)) if resp.status().is_success() => return Ok(resp),
                Ok(Ok(resp)) => {
                    let status = resp.status();
                    let text = redact(&resp.text().await.unwrap_or_default());
                    let err = anyhow::anyhow!("LLM: HTTP {status}: {text}");
                    if !(status == reqwest::StatusCode::TOO_MANY_REQUESTS
                        || status.is_server_error())
                        || attempt == self.retries
                    {
                        return Err(err);
                    }
                    last = Some(err);
                }
                Ok(Err(e)) if attempt == self.retries => return Err(e).context("LLM: запрос"),
                Ok(Err(e)) => last = Some(anyhow::Error::from(e)),
                Err(_) if attempt == self.retries => {
                    bail!("LLM: тайм-аут {} мс", self.timeout.as_millis())
                }
                Err(_) => last = Some(anyhow::anyhow!("LLM: тайм-аут")),
            }
            tracing::warn!("LLM: повтор {}/{}", attempt + 1, self.retries);
            tokio::time::sleep(Duration::from_millis(400 * 2u64.pow(attempt as u32))).await;
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
        let body = self.body(&messages, &tools);
        let resp = self.send(body).await?;
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
    use crate::testutil::serve_once;

    fn cfg(url: &str, stream: bool) -> LlmConfig {
        LlmConfig {
            base_url: url.into(),
            stream,
            timeout_ms: 5000,
            retries: 0,
            ..LlmConfig::default()
        }
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
}
