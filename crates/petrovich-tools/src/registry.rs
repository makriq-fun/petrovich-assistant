//! Реестр инструментов: единая точка проверки разрешений, тайм-аутов и
//! выполнения. LLM не получает доступ к оболочке напрямую.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{Datelike, Local, Timelike};
use petrovich_core::chat::ToolDefinition;
use petrovich_core::config::ToolsConfig;
use petrovich_core::ports::{MusicControl, VolumeControl};
use serde_json::{json, Value};
use tokio::process::Command;

use crate::timers::TimerManager;
use crate::weather::Weather;
use crate::web::Web;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlAction {
    StopSpeaking,
    ResetDialog,
}

#[derive(Debug)]
pub struct ToolResult {
    pub content: String,
    pub control: Option<ControlAction>,
}

impl ToolResult {
    fn text(s: impl Into<String>) -> Self {
        Self {
            content: s.into(),
            control: None,
        }
    }
}

#[derive(Clone)]
pub struct ToolContext {
    volume: Arc<dyn VolumeControl>,
    music: Option<Arc<dyn MusicControl>>,
    timers: TimerManager,
    web: Web,
    weather: Option<Weather>,
    cfg: ToolsConfig,
    volume_step: i32,
}

impl ToolContext {
    pub fn new(
        volume: Arc<dyn VolumeControl>,
        music: Option<Arc<dyn MusicControl>>,
        timers: TimerManager,
        web: Web,
        cfg: ToolsConfig,
        volume_step: i32,
    ) -> Self {
        Self {
            volume,
            music,
            timers,
            web,
            weather: None,
            cfg,
            volume_step: volume_step.clamp(1, 50),
        }
    }

    pub fn with_weather(mut self, weather: Weather) -> Self {
        self.weather = Some(weather);
        self
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.names()
            .into_iter()
            .filter_map(|n| self.definition(n))
            .collect()
    }

    fn names(&self) -> Vec<&'static str> {
        vec![
            "time",
            "timer",
            "volume",
            "music",
            "web_search",
            "web_fetch",
            "weather",
            "run_code",
            "reset_dialog",
            "stop",
        ]
    }

    fn enabled(&self, name: &str) -> bool {
        self.cfg.enabled.iter().any(|t| t == name)
    }

    fn available(&self, name: &str) -> bool {
        match name {
            "music" => self.music.is_some(),
            "web_search" => !self.cfg.search_url.is_empty(),
            "weather" => self.weather.is_some(),
            "run_code" => !self.cfg.sandbox_command.is_empty(),
            _ => true,
        }
    }

    fn definition(&self, name: &str) -> Option<ToolDefinition> {
        if !self.enabled(name) || !self.available(name) {
            return None;
        }
        let obj = |props: Value, required: Value| json!({"type":"object","properties":props,"required":required,"additionalProperties":false});
        Some(match name {
            "time" => ToolDefinition::new(
                "time",
                "Текущие дата и время на колонке",
                obj(json!({}), json!([])),
            ),
            "timer" => ToolDefinition::new(
                "timer",
                "Поставить таймер. Секунды от 1 до 86400.",
                obj(
                    json!({
                        "seconds": {"type":"integer","minimum":1,"maximum":86400},
                        "label": {"type":"string","maxLength":80}
                    }),
                    json!(["seconds"]),
                ),
            ),
            "volume" => ToolDefinition::new(
                "volume",
                "Громкость колонки: set (0..100), up, down, get",
                obj(
                    json!({
                        "action": {"type":"string","enum":["set","up","down","get"]},
                        "value": {"type":"integer","minimum":0,"maximum":100}
                    }),
                    json!(["action"]),
                ),
            ),
            "music" => ToolDefinition::new(
                "music",
                "Управление музыкой через Spotiya",
                obj(
                    json!({"action":{"type":"string","enum":["play","pause","resume","next","previous","status"]}}),
                    json!(["action"]),
                ),
            ),
            "web_search" => ToolDefinition::new(
                "web_search",
                "Поиск в интернете (SearXNG)",
                obj(
                    json!({"query":{"type":"string","minLength":1,"maxLength":300}}),
                    json!(["query"]),
                ),
            ),
            "web_fetch" => ToolDefinition::new(
                "web_fetch",
                "Прочитать текст HTTPS-страницы",
                obj(
                    json!({"url":{"type":"string","minLength":8,"maxLength":2000}}),
                    json!(["url"]),
                ),
            ),
            "weather" => ToolDefinition::new(
                "weather",
                "Текущая погода и прогноз на день для настроенного места",
                obj(json!({}), json!([])),
            ),
            "run_code" => ToolDefinition::new(
                "run_code",
                "Выполнить код в настроенной администратором песочнице",
                obj(
                    json!({"code":{"type":"string","minLength":1,"maxLength":20000}}),
                    json!(["code"]),
                ),
            ),
            "reset_dialog" => ToolDefinition::new(
                "reset_dialog",
                "Забыть историю диалога",
                obj(json!({}), json!([])),
            ),
            "stop" => ToolDefinition::new(
                "stop",
                "Прекратить текущий ответ и слушать",
                obj(json!({}), json!([])),
            ),
            _ => return None,
        })
    }

    pub async fn execute(&self, name: &str, args: &str) -> Result<ToolResult> {
        if !self.enabled(name) {
            bail!("инструмент {name} запрещён конфигурацией");
        }
        if !self.available(name) {
            bail!("инструмент {name} не настроен");
        }
        let args: Value = if args.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(args).with_context(|| format!("{name}: аргументы не JSON"))?
        };
        let timeout = match name {
            "web_fetch" | "web_search" | "weather" => {
                Duration::from_millis(self.cfg.web_timeout_ms)
            }
            "run_code" => Duration::from_millis(self.cfg.sandbox_timeout_ms),
            _ => Duration::from_millis(self.cfg.timeout_ms),
        };
        let fut = self.dispatch(name, &args);
        match tokio::time::timeout(timeout, fut).await {
            Ok(r) => r,
            Err(_) => bail!("{name}: тайм-аут {} мс", timeout.as_millis()),
        }
    }

    async fn dispatch(&self, name: &str, args: &Value) -> Result<ToolResult> {
        match name {
            "time" => {
                let now = Local::now();
                let weekdays = [
                    "понедельник",
                    "вторник",
                    "среда",
                    "четверг",
                    "пятница",
                    "суббота",
                    "воскресенье",
                ];
                let months = [
                    "января",
                    "февраля",
                    "марта",
                    "апреля",
                    "мая",
                    "июня",
                    "июля",
                    "августа",
                    "сентября",
                    "октября",
                    "ноября",
                    "декабря",
                ];
                Ok(ToolResult::text(format!(
                    "Сейчас {:02}:{:02}, {} {} {} {} года.",
                    now.hour(),
                    now.minute(),
                    weekdays[now.weekday().num_days_from_monday() as usize],
                    now.day(),
                    months[now.month0() as usize],
                    now.year()
                )))
            }
            "timer" => {
                let seconds = args
                    .get("seconds")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| anyhow::anyhow!("timer: нет seconds"))?;
                if !(1..=86_400).contains(&seconds) {
                    bail!("timer: seconds вне 1..86400");
                }
                let label = args
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("таймер")
                    .chars()
                    .take(80)
                    .collect::<String>();
                let id = self.timers.add(seconds, label.clone()).await;
                Ok(ToolResult::text(format!(
                    "Таймер «{label}» на {seconds} с запущен (id {id})."
                )))
            }
            "volume" => {
                let action = args.get("action").and_then(Value::as_str).unwrap_or("get");
                let status = match action {
                    "set" => {
                        let v = args
                            .get("value")
                            .and_then(Value::as_i64)
                            .ok_or_else(|| anyhow::anyhow!("volume: нет value"))?;
                        self.volume.set_percent(v.clamp(0, 100) as i32)?
                    }
                    "up" => self.volume.step(self.volume_step)?,
                    "down" => self.volume.step(-self.volume_step)?,
                    "get" => self.volume.get()?,
                    _ => bail!("volume: неизвестное действие {action}"),
                };
                Ok(ToolResult::text(format!(
                    "Громкость {}%{}.",
                    status.percent,
                    if status.muted {
                        ", звук выключен"
                    } else {
                        ""
                    }
                )))
            }
            "music" => {
                let music = self
                    .music
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("music не настроен"))?;
                let action = args
                    .get("action")
                    .and_then(Value::as_str)
                    .unwrap_or("status");
                match action {
                    "play" => music.play()?,
                    "pause" => music.pause()?,
                    "resume" => music.resume()?,
                    "next" => music.next()?,
                    "previous" => music.previous()?,
                    "status" => return Ok(ToolResult::text(music.status()?)),
                    _ => bail!("music: неизвестное действие {action}"),
                }
                Ok(ToolResult::text("Готово."))
            }
            "web_fetch" => {
                let url = args
                    .get("url")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("web_fetch: нет url"))?;
                let page = self.web.fetch(url).await?;
                let mut out = String::new();
                if let Some(t) = page.title {
                    out.push_str(&format!("# {t}\n"));
                }
                out.push_str(&format!("{}\n\n{}", page.url, page.text));
                Ok(ToolResult::text(out))
            }
            "web_search" => {
                let query = args
                    .get("query")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("web_search: нет query"))?;
                let hits = self.web.search(&self.cfg.search_url, query).await?;
                if hits.is_empty() {
                    return Ok(ToolResult::text("Ничего не найдено."));
                }
                let mut out = String::new();
                for (i, h) in hits.iter().enumerate() {
                    out.push_str(&format!(
                        "{}. {}\n{}\n{}\n\n",
                        i + 1,
                        h.title,
                        h.url,
                        h.snippet
                    ));
                }
                Ok(ToolResult::text(out))
            }
            "weather" => {
                let w = self
                    .weather
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("погода не настроена"))?;
                Ok(ToolResult::text(w.current().await?))
            }
            "run_code" => {
                if self.cfg.sandbox_command.is_empty() {
                    bail!("run_code: песочница не настроена");
                }
                let code = args
                    .get("code")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("run_code: нет code"))?;
                let argv = &self.cfg.sandbox_command;
                let out = Command::new(&argv[0])
                    .args(&argv[1..])
                    .arg(code)
                    .kill_on_drop(true)
                    .output()
                    .await
                    .with_context(|| format!("run_code: {}", argv[0]))?;
                let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                let err = String::from_utf8_lossy(&out.stderr);
                if !err.is_empty() {
                    text.push_str("\nstderr: ");
                    text.push_str(&err);
                }
                if text.chars().count() > 16_000 {
                    text = text.chars().take(16_000).collect::<String>() + "…";
                }
                if !out.status.success() {
                    text = format!("Код {}.\n{text}", out.status);
                }
                Ok(ToolResult::text(text.trim().to_owned()))
            }
            "reset_dialog" => Ok(ToolResult {
                content: "История диалога очищена.".into(),
                control: Some(ControlAction::ResetDialog),
            }),
            "stop" => Ok(ToolResult {
                content: "Остановлено.".into(),
                control: Some(ControlAction::StopSpeaking),
            }),
            _ => bail!("неизвестный инструмент {name}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use petrovich_core::ports::VolumeStatus;
    use tokio::sync::mpsc;

    struct FakeVolume;
    impl VolumeControl for FakeVolume {
        fn get(&self) -> petrovich_core::Result<VolumeStatus> {
            Ok(VolumeStatus {
                percent: 40,
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
                percent: 40 + d,
                muted: false,
            })
        }
    }

    struct FakeMusic;
    impl MusicControl for FakeMusic {
        fn play(&self) -> petrovich_core::Result<()> {
            Ok(())
        }
        fn pause(&self) -> petrovich_core::Result<()> {
            Ok(())
        }
        fn resume(&self) -> petrovich_core::Result<()> {
            Ok(())
        }
        fn next(&self) -> petrovich_core::Result<()> {
            Ok(())
        }
        fn previous(&self) -> petrovich_core::Result<()> {
            Ok(())
        }
        fn status(&self) -> petrovich_core::Result<String> {
            Ok("играет".into())
        }
    }

    fn ctx(enabled: &[&str]) -> ToolContext {
        let (ui, _) = mpsc::unbounded_channel();
        let (ann, _) = mpsc::channel(4);
        let cfg = ToolsConfig {
            enabled: enabled.iter().map(|s| s.to_string()).collect(),
            ..ToolsConfig::default()
        };
        ToolContext::new(
            Arc::new(FakeVolume),
            Some(Arc::new(FakeMusic)),
            TimerManager::new(ui, ann),
            Web::new(5000, 100_000, true).unwrap(),
            cfg,
            5,
        )
    }

    #[tokio::test]
    async fn disabled_tool_is_forbidden() {
        let c = ctx(&["time"]);
        assert!(c.execute("volume", r#"{"action":"get"}"#).await.is_err());
        assert!(c.definitions().iter().all(|d| d.function.name != "volume"));
    }

    #[tokio::test]
    async fn time_and_volume_work() {
        let c = ctx(&["time", "volume"]);
        let t = c.execute("time", "{}").await.unwrap();
        assert!(t.content.contains("Сейчас"));
        let v = c
            .execute("volume", r#"{"action":"set","value":55}"#)
            .await
            .unwrap();
        assert!(v.content.contains("55%"));
        let v = c.execute("volume", r#"{"action":"up"}"#).await.unwrap();
        assert!(v.content.contains("45%"));
    }

    #[tokio::test]
    async fn timer_and_music() {
        let c = ctx(&["timer", "music"]);
        let r = c
            .execute("timer", r#"{"seconds":10,"label":"чай"}"#)
            .await
            .unwrap();
        assert!(r.content.contains("10"));
        assert_eq!(c.timers().await, 1);
        assert_eq!(
            c.execute("music", r#"{"action":"status"}"#)
                .await
                .unwrap()
                .content,
            "играет"
        );
    }

    #[tokio::test]
    async fn control_actions_are_returned() {
        let c = ctx(&["stop", "reset_dialog"]);
        assert_eq!(
            c.execute("stop", "{}").await.unwrap().control,
            Some(ControlAction::StopSpeaking)
        );
        assert_eq!(
            c.execute("reset_dialog", "{}").await.unwrap().control,
            Some(ControlAction::ResetDialog)
        );
    }

    #[tokio::test]
    async fn run_code_requires_sandbox() {
        let c = ctx(&["run_code"]);
        assert!(c
            .execute("run_code", r#"{"code":"echo hi"}"#)
            .await
            .is_err());
        assert!(c
            .definitions()
            .iter()
            .all(|d| d.function.name != "run_code"));
    }

    #[tokio::test]
    async fn weather_tool_is_available_when_configured() {
        let (url, handle) = crate::testutil::serve_once(
            200,
            "application/json",
            r#"{"current":{"temperature_2m":5.0,"weather_code":0},"daily":{"temperature_2m_max":[7.0],"temperature_2m_min":[1.0]}}"#,
        )
        .await;
        let w = crate::weather::Weather::new(url, 55.7, 37.6, "Москва", 5000, true).unwrap();
        let c = ctx(&["weather"]).with_weather(w);
        let r = c.execute("weather", "{}").await.unwrap();
        assert!(r.content.contains("Москва"));
        assert!(r.content.contains("ясно"));
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn web_fetch_works_with_local_server() {
        let (url, handle) = crate::testutil::serve_once(
            200,
            "text/html",
            "<html><title>T</title><body><p>Текст</p></body></html>",
        )
        .await;
        let c = ctx(&["web_fetch"]);
        let r = c
            .execute("web_fetch", &format!(r#"{{"url":"{url}"}}"#))
            .await
            .unwrap();
        assert!(r.content.contains("Текст"));
        handle.await.unwrap();
    }

    impl ToolContext {
        async fn timers(&self) -> usize {
            self.timers.count().await
        }
    }
}
