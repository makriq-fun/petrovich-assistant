//! Один ход диалога: запись уже собрана VAD. Здесь STT, LLM с инструментами,
//! потоковый TTS и воспроизведение. Задача прерывается abort() при бардж-ине.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use futures_util::StreamExt;
use petrovich_core::chat::{ChatMessage, LlmEvent, ToolCall};
use petrovich_core::config::TtsFormat;
use petrovich_core::events::UiEvent;
use petrovich_core::text::SentenceChunker;
use petrovich_core::wav;
use petrovich_providers::llm::OpenAiLlm;
use petrovich_providers::stt::Stt;
use petrovich_providers::tts::{AudioStream, Tts};
use petrovich_tools::{ControlAction, ToolContext};
use tokio::sync::mpsc::UnboundedSender;

use petrovich_audio::frontend::PlaybackHandle;

use crate::tts_cache::{CachedAudio, TtsCache};

#[derive(Clone)]
pub struct TurnContext {
    pub llm: Arc<OpenAiLlm>,
    pub stt: Arc<dyn Stt>,
    pub tts: Arc<dyn Tts>,
    pub tts_cache: TtsCache,
    pub tools: ToolContext,
    pub playback: PlaybackHandle,
    pub ui: UnboundedSender<UiEvent>,
    pub max_tool_rounds: u8,
}

pub struct TurnOutcome {
    pub messages: Vec<ChatMessage>,
    pub spoken: String,
    pub control: Option<ControlAction>,
}

/// Выполняет распознавание и ответ. `system_and_history` — снимок истории
/// без текущей реплики.
pub async fn run_turn(
    ctx: &TurnContext,
    system_and_history: Vec<ChatMessage>,
    pcm: Vec<i16>,
) -> Result<TurnOutcome> {
    let wav = wav::encode_mono(16_000, &pcm);
    let heard = ctx.stt.transcribe(wav).await.context("распознавание")?;
    let text = strip_wake_word(&heard);
    if text.is_empty() || is_stt_artifact(&text) {
        bail!("не расслышал");
    }
    tracing::info!("запрос: {text}");
    let mut new_messages = vec![ChatMessage::user(text.clone())];
    if is_stop_phrase(&text) {
        tracing::info!("локальная команда остановки");
        return Ok(TurnOutcome {
            messages: new_messages,
            spoken: String::new(),
            control: Some(ControlAction::StopSpeaking),
        });
    }
    let mut messages = system_and_history;
    messages.extend(new_messages.clone());
    let tools = ctx.tools.definitions();
    let mut spoken = String::new();
    let mut control = None;
    let mut final_text: Option<String> = None;
    let mut audio_total = Duration::ZERO;
    let mut audio_started: Option<tokio::time::Instant> = None;
    // Одинаковые вызовы в одном ходе не выполняем повторно: модель этим
    // забивает лимит раундов и обрывает ответ.
    let mut seen_calls = std::collections::HashSet::new();
    let final_round = ctx.max_tool_rounds.saturating_add(1);
    for round in 0..=final_round {
        // После лимита ходов всё равно просим итог, но уже без схем
        // инструментов: иначе модель повторяет вызовы, а пользователь
        // остаётся без ответа.
        let force_final = round == final_round;
        if force_final {
            tracing::warn!("лимит вызовов инструментов; финальный ответ без инструментов");
        }
        let round_tools = if force_final {
            Vec::new()
        } else {
            tools.clone()
        };
        let mut rx = ctx
            .llm
            .chat_stream(messages.clone(), round_tools)
            .await
            .context("LLM")?;
        let mut chunker = SentenceChunker::default();
        let mut round_spoken = String::new();
        let mut calls: Option<Vec<ToolCall>> = None;
        while let Some(ev) = rx.recv().await {
            match ev {
                LlmEvent::TextDelta(t) => {
                    let mut sentences = Vec::new();
                    chunker.push(&t, &mut sentences);
                    for s in sentences {
                        let d = speak_sentence(ctx, &s).await?;
                        audio_total += d;
                        if !d.is_zero() {
                            audio_started.get_or_insert_with(tokio::time::Instant::now);
                        }
                        spoken.push_str(&s);
                        spoken.push(' ');
                        round_spoken.push_str(&s);
                        round_spoken.push(' ');
                    }
                }
                LlmEvent::ToolCalls(c) => {
                    calls = Some(c);
                }
                LlmEvent::Error(e) => bail!("{e}"),
                LlmEvent::Done { .. } => break,
            }
        }
        if let Some(s) = chunker.flush() {
            let d = speak_sentence(ctx, &s).await?;
            audio_total += d;
            if !d.is_zero() {
                audio_started.get_or_insert_with(tokio::time::Instant::now);
            }
            spoken.push_str(&s);
            spoken.push(' ');
            round_spoken.push_str(&s);
            round_spoken.push(' ');
        }
        if chunker.code_seen() && !round_spoken.contains("Код не пишу.") {
            let refusal = "Код не пишу.";
            let d = speak_sentence(ctx, refusal).await?;
            audio_total += d;
            if !d.is_zero() {
                audio_started.get_or_insert_with(tokio::time::Instant::now);
            }
            spoken.push_str(refusal);
            spoken.push(' ');
            round_spoken.push_str(refusal);
            round_spoken.push(' ');
        }
        match calls {
            Some(calls) if !calls.is_empty() => {
                if force_final {
                    tracing::warn!("лимит вызовов инструментов; отвечаю тем, что уже сказано");
                    if !round_spoken.trim().is_empty() {
                        final_text = Some(round_spoken.trim().to_owned());
                    }
                    break;
                }
                let assistant = ChatMessage {
                    role: petrovich_core::chat::Role::Assistant,
                    content: (!round_spoken.trim().is_empty())
                        .then(|| round_spoken.trim().to_owned()),
                    tool_calls: Some(calls.clone()),
                    tool_call_id: None,
                    name: None,
                };
                messages.push(assistant.clone());
                new_messages.push(assistant);
                for call in &calls {
                    // Громкость/таймер можно вызвать повторно с теми же
                    // аргументами; дедуплицируем только сетевые запросы.
                    let dedup = matches!(
                        call.function.name.as_str(),
                        "youtube" | "web_search" | "web_fetch" | "weather" | "sugar"
                    );
                    let first = !dedup
                        || seen_calls
                            .insert((call.function.name.clone(), call.function.arguments.clone()));
                    let (content, ctrl) = if !first {
                        (
                            "Повторный вызов с теми же аргументами проигнорирован.".to_owned(),
                            None,
                        )
                    } else {
                        match ctx
                            .tools
                            .execute(&call.function.name, &call.function.arguments)
                            .await
                        {
                            Ok(r) => (r.content, r.control),
                            Err(e) => (format!("Ошибка инструмента: {e:#}"), None),
                        }
                    };
                    tracing::info!(
                        "инструмент {}: {}",
                        call.function.name,
                        content.chars().take(300).collect::<String>()
                    );
                    if control.is_none() {
                        control = ctrl;
                    }
                    let msg = ChatMessage::tool_result(&call.id, &call.function.name, content);
                    messages.push(msg.clone());
                    new_messages.push(msg);
                }
                if control.is_some() {
                    break;
                }
            }
            _ => {
                if !round_spoken.trim().is_empty() {
                    final_text = Some(round_spoken.trim().to_owned());
                }
                break;
            }
        }
    }
    if let Some(text) = final_text {
        new_messages.push(ChatMessage::assistant(text));
    }
    if let Some(start) = audio_started {
        let left = audio_total.saturating_sub(start.elapsed());
        if !left.is_zero() {
            tokio::time::sleep(left).await;
        }
        let _ = tokio::time::timeout(Duration::from_secs(3), ctx.playback.wait_idle()).await;
        let _ = ctx.ui.send(UiEvent::SpeechFinished);
    }
    Ok(TurnOutcome {
        messages: new_messages,
        spoken: spoken.trim().to_owned(),
        control,
    })
}

/// Озвучивает готовый текст (например, сработавший таймер). Следующая фраза
/// синтезируется, пока играет текущая; готовые фразы берутся из кэша.
pub async fn speak_text(ctx: &TurnContext, text: &str) -> Result<()> {
    let mut chunker = SentenceChunker::default();
    let mut sentences = Vec::new();
    chunker.push(text, &mut sentences);
    if let Some(s) = chunker.flush() {
        sentences.push(s);
    }
    let mut iter = sentences.into_iter();
    let mut pending = iter.next().map(|s| (s.clone(), spawn_prepare(ctx, s)));
    let mut total = Duration::ZERO;
    let start = tokio::time::Instant::now();
    while let Some((sentence, task)) = pending {
        let prepared = task.await.context("TTS: задача")??;
        pending = iter.next().map(|s| (s.clone(), spawn_prepare(ctx, s)));
        total += speak_prepared(ctx, &sentence, prepared).await?;
    }
    let left = total.saturating_sub(start.elapsed());
    if !left.is_zero() {
        tokio::time::sleep(left).await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(3), ctx.playback.wait_idle()).await;
    let _ = ctx.ui.send(UiEvent::SpeechFinished);
    Ok(())
}

enum Prepared {
    Cached(CachedAudio),
    Live(AudioStream),
}

fn spawn_prepare(ctx: &TurnContext, text: String) -> tokio::task::JoinHandle<Result<Prepared>> {
    let tts = ctx.tts.clone();
    let cache = ctx.tts_cache.clone();
    tokio::spawn(async move {
        let text = text.trim().to_owned();
        if let Some(c) = cache.get(&text).await {
            return Ok(Prepared::Cached(c));
        }
        let audio = tts.synthesize(&text).await.context("TTS")?;
        Ok(Prepared::Live(audio))
    })
}

async fn speak_prepared(ctx: &TurnContext, text: &str, prepared: Prepared) -> Result<Duration> {
    if ctx.ui.send(UiEvent::SpeechStarted).is_err() {
        return Ok(Duration::ZERO);
    }
    match prepared {
        Prepared::Cached(c) => stream_audio(c.to_stream(), &ctx.playback).await,
        Prepared::Live(audio) => {
            let (format, duration, bytes, sample_rate, channels) =
                stream_audio_capture(audio, &ctx.playback).await?;
            if !bytes.is_empty() {
                ctx.tts_cache
                    .put(
                        text,
                        CachedAudio {
                            format,
                            sample_rate,
                            channels,
                            bytes: Bytes::from(bytes),
                        },
                    )
                    .await;
            }
            Ok(duration)
        }
    }
}

async fn speak_sentence(ctx: &TurnContext, text: &str) -> Result<Duration> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(Duration::ZERO);
    }
    if ctx.ui.send(UiEvent::SpeechStarted).is_err() {
        return Ok(Duration::ZERO);
    }
    let audio = ctx.tts.synthesize(text).await.context("TTS")?;
    stream_audio(audio, &ctx.playback).await
}

/// Поток TTS нельзя клонировать; потребляем его по значению.
async fn stream_audio(audio: AudioStream, playback: &PlaybackHandle) -> Result<Duration> {
    Ok(stream_audio_capture(audio, playback).await?.1)
}

/// Проигрывает поток и заодно запоминает исходные PCM-байты для кэша.
async fn stream_audio_capture(
    audio: AudioStream,
    playback: &PlaybackHandle,
) -> Result<(TtsFormat, Duration, Vec<u8>, u32, u16)> {
    let AudioStream {
        format,
        sample_rate,
        channels,
        stream,
    } = audio;
    let rate = sample_rate.max(1);
    let ch = channels.max(1) as usize;
    let mut carry: Option<u8> = None;
    let mut captured = Vec::new();
    let mut total = Duration::ZERO;
    let mut stream = stream;
    while let Some(chunk) = stream.next().await {
        let bytes: Bytes = chunk?;
        captured.extend_from_slice(&bytes);
        let mut samples = Vec::with_capacity(bytes.len() / 2 + 1);
        let mut it = bytes.iter().copied();
        while let Some(b0) = carry.take().or_else(|| it.next()) {
            let Some(b1) = it.next() else {
                carry = Some(b0);
                break;
            };
            samples.push(i16::from_le_bytes([b0, b1]));
        }
        if ch > 1 {
            let mono: Vec<i16> = samples
                .chunks_exact(ch)
                .map(|f| {
                    let sum: i32 = f.iter().map(|v| i32::from(*v)).sum();
                    (sum / ch as i32) as i16
                })
                .collect();
            total += playback.play_pcm(mono, rate);
        } else {
            total += playback.play_pcm(samples, rate);
        }
    }
    Ok((format, total, captured, rate, ch as u16))
}

/// Убирает обращение «Петрович» в начале распознанной фразы.
pub fn strip_wake_word(text: &str) -> String {
    let t = text.trim();
    let mut words = t.split_whitespace();
    let Some(first) = words.next() else {
        return String::new();
    };
    let clean = first
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase();
    if matches!(clean.as_str(), "петрович" | "petrovich") {
        words.collect::<Vec<_>>().join(" ")
    } else {
        t.to_owned()
    }
}

/// Короткие команды остановки обрабатываются без LLM, чтобы реакция была
/// мгновенной и не зависела от сети.
fn is_stop_phrase(text: &str) -> bool {
    let t = text
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>();
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    matches!(
        t.as_str(),
        "стоп"
            | "хватит"
            | "замолчи"
            | "замолчи пожалуйста"
            | "отмена"
            | "отмени"
            | "остановись"
            | "тихо"
            | "заткнись"
            | "stop"
            | "quiet"
    )
}

fn is_stt_artifact(text: &str) -> bool {
    let t = text.to_lowercase();
    [
        "продолжение следует",
        "редактор субтитров",
        "корректор",
        "субтитры создавал",
        "dima torzok",
        "dimatorzok",
        "спасибо за просмотр",
    ]
    .iter()
    .any(|p| t.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_russian_and_english_wake_word() {
        assert_eq!(strip_wake_word("Петрович, какая погода?"), "какая погода?");
        assert_eq!(strip_wake_word("petrovich what time"), "what time");
        assert_eq!(strip_wake_word("погода"), "погода");
        assert_eq!(strip_wake_word("Петрович"), "");
        assert!(is_stt_artifact("Продолжение следует..."));
        assert!(!is_stt_artifact("Какая сегодня погода?"));
    }

    #[test]
    fn recognizes_stop_phrases() {
        for s in ["Стоп", "стоп!", "Хватит.", "Замолчи, пожалуйста", "отмена"]
        {
            assert!(is_stop_phrase(s), "{s}");
        }
        for s in ["останови таймер", "стоп-кран", "какая погода"] {
            assert!(!is_stop_phrase(s), "{s}");
        }
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use petrovich_core::chat::FunctionCall;
    use petrovich_core::chat::ToolDefinition;
    use petrovich_core::config::{LlmConfig, ToolsConfig};
    use petrovich_core::ports::{Chime, ChimeSound, VolumeControl, VolumeStatus};
    use petrovich_core::Secrets;
    use petrovich_providers::tts::AudioStream;
    use petrovich_tools::{AlarmManager, TimerManager, ToolContext, Web};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    struct FakeStt;
    #[async_trait::async_trait]
    impl Stt for FakeStt {
        async fn transcribe(&self, _wav: Vec<u8>) -> Result<String> {
            Ok("Петрович, который час".into())
        }
    }

    struct FakeTts;
    #[async_trait::async_trait]
    impl Tts for FakeTts {
        async fn synthesize(&self, _text: &str) -> Result<AudioStream> {
            Ok(AudioStream {
                format: petrovich_core::config::TtsFormat::Pcm,
                sample_rate: 24_000,
                channels: 1,
                stream: futures_util::stream::empty().boxed(),
            })
        }
        fn audio_format(&self) -> petrovich_core::config::TtsFormat {
            petrovich_core::config::TtsFormat::Pcm
        }
        fn sample_rate(&self) -> u32 {
            24_000
        }
    }

    struct CountingTts {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Tts for CountingTts {
        async fn synthesize(&self, _text: &str) -> Result<AudioStream> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(AudioStream {
                format: petrovich_core::config::TtsFormat::Pcm,
                sample_rate: 24_000,
                channels: 1,
                stream: futures_util::stream::once(async { Ok(Bytes::from_static(&[0, 0, 0, 0])) })
                    .boxed(),
            })
        }
        fn audio_format(&self) -> petrovich_core::config::TtsFormat {
            petrovich_core::config::TtsFormat::Pcm
        }
        fn sample_rate(&self) -> u32 {
            24_000
        }
    }

    struct FakeChime;
    impl Chime for FakeChime {
        fn play(&self, _sound: ChimeSound) -> petrovich_core::Result<()> {
            Ok(())
        }
    }

    struct FakeVolume;
    impl VolumeControl for FakeVolume {
        fn get(&self) -> petrovich_core::Result<VolumeStatus> {
            Ok(VolumeStatus {
                percent: 50,
                muted: false,
            })
        }
        fn set_percent(&self, p: i32) -> petrovich_core::Result<VolumeStatus> {
            Ok(VolumeStatus {
                percent: p,
                muted: false,
            })
        }
        fn step(&self, d: i32) -> petrovich_core::Result<VolumeStatus> {
            Ok(VolumeStatus {
                percent: 50 + d,
                muted: false,
            })
        }
    }

    async fn serve(responses: Vec<&'static str>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for body in responses {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        format!("http://{addr}")
    }

    fn ctx(url: String) -> TurnContext {
        let cfg = LlmConfig {
            base_url: url,
            stream: true,
            retries: 0,
            timeout_ms: 5000,
            ..LlmConfig::default()
        };
        let llm = Arc::new(OpenAiLlm::new(&cfg, &Secrets::default()).unwrap());
        let (ui, _ui_rx) = mpsc::unbounded_channel();
        let (ann, _ann_rx) = mpsc::channel(4);
        let (alarm_ann, _alarm_rx) = mpsc::channel(4);
        let timers = TimerManager::new(ui.clone(), ann);
        let alarms = AlarmManager::new(ui.clone(), alarm_ann);
        let tools_cfg = ToolsConfig {
            enabled: vec!["time".into()],
            ..ToolsConfig::default()
        };
        let tools = ToolContext::new(
            Arc::new(FakeVolume),
            None,
            timers,
            alarms,
            Web::new(5000, 100_000, true).unwrap(),
            Arc::new(FakeChime),
            ui.clone(),
            tools_cfg,
            5,
        );
        TurnContext {
            llm,
            stt: Arc::new(FakeStt),
            tts: Arc::new(FakeTts),
            tts_cache: crate::tts_cache::TtsCache::default(),
            tools,
            playback: PlaybackHandle::detached(),
            ui,
            max_tool_rounds: 2,
        }
    }

    #[tokio::test]
    async fn speak_text_caches_phrase() {
        let mut c = ctx("http://127.0.0.1:1".into());
        let (ui, _rx) = mpsc::unbounded_channel();
        c.ui = ui;
        let calls = Arc::new(AtomicUsize::new(0));
        c.tts = Arc::new(CountingTts {
            calls: calls.clone(),
        });
        speak_text(&c, "Таймер сработал.").await.unwrap();
        speak_text(&c, "Таймер сработал.").await.unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(c.tts_cache.len().await, 1);
    }

    #[tokio::test]
    async fn text_turn_returns_history() {
        let url = serve(vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"Сейчас полдень.\"}}]}\n\ndata: [DONE]\n\n",
        ])
        .await;
        let c = ctx(url);
        let out = run_turn(&c, vec![ChatMessage::system("s")], vec![0; 1600])
            .await
            .unwrap();
        assert!(out.spoken.contains("полдень"));
        assert_eq!(out.messages.len(), 2);
        assert_eq!(out.messages[1].content.as_deref(), Some("Сейчас полдень."));
    }

    #[tokio::test]
    async fn tool_call_round_trip() {
        let url = serve(vec![
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"time\",\"arguments\":\"{}\"}}]}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Сейчас 12:00.\"}}]}\n\ndata: [DONE]\n\n",
        ])
        .await;
        let c = ctx(url);
        let out = run_turn(&c, vec![ChatMessage::system("s")], vec![0; 1600])
            .await
            .unwrap();
        assert!(out.spoken.contains("12:00"));
        // user, assistant(tool_calls), tool, assistant(final)
        assert_eq!(out.messages.len(), 4);
        assert_eq!(out.messages[2].role, petrovich_core::chat::Role::Tool);
        let _ = FunctionCall {
            name: "time".into(),
            arguments: "{}".into(),
        };
        let _ = ToolDefinition::new("x", "x", serde_json::json!({}));
    }

    #[tokio::test]
    async fn tool_limit_still_answers() {
        let call = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"time\",\"arguments\":\"{}\"}}]}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n";
        let url = serve(vec![
            call,
            call,
            "data: {\"choices\":[{\"delta\":{\"content\":\"Уже поздно.\"}}]}\n\ndata: [DONE]\n\n",
        ])
        .await;
        let c = ctx(url);
        let out = run_turn(&c, vec![ChatMessage::system("s")], vec![0; 1600])
            .await
            .unwrap();
        assert!(out.spoken.contains("поздно"));
    }
}
