//! Реестр инструментов: единая точка проверки разрешений, тайм-аутов и
//! выполнения. LLM не получает доступ к оболочке напрямую.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{Datelike, Local, NaiveTime, Timelike};
use petrovich_core::chat::ToolDefinition;
use petrovich_core::config::ToolsConfig;
use petrovich_core::events::{Emotion, UiEvent};
use petrovich_core::memory::SharedMemory;
use petrovich_core::ports::{Chime, ChimeSound, MusicControl, VolumeControl, YouTubeControl};
use serde_json::{json, Value};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::nightscout::Nightscout;
use crate::timers::{AlarmManager, TimerManager};
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
    youtube: Option<Arc<dyn YouTubeControl>>,
    timers: TimerManager,
    alarms: AlarmManager,
    web: Web,
    weather: Option<Weather>,
    nightscout: Option<Nightscout>,
    memory: Option<SharedMemory>,
    chime: Arc<dyn Chime>,
    ui: mpsc::UnboundedSender<UiEvent>,
    cfg: ToolsConfig,
    volume_step: i32,
}

impl ToolContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        volume: Arc<dyn VolumeControl>,
        music: Option<Arc<dyn MusicControl>>,
        timers: TimerManager,
        alarms: AlarmManager,
        web: Web,
        chime: Arc<dyn Chime>,
        ui: mpsc::UnboundedSender<UiEvent>,
        cfg: ToolsConfig,
        volume_step: i32,
    ) -> Self {
        Self {
            volume,
            music,
            youtube: None,
            timers,
            alarms,
            web,
            weather: None,
            nightscout: None,
            memory: None,
            chime,
            ui,
            cfg,
            volume_step: volume_step.clamp(1, 50),
        }
    }

    pub fn with_weather(mut self, weather: Weather) -> Self {
        self.weather = Some(weather);
        self
    }

    pub fn with_nightscout(mut self, nightscout: Nightscout) -> Self {
        self.nightscout = Some(nightscout);
        self
    }

    pub fn with_youtube(mut self, youtube: Arc<dyn YouTubeControl>) -> Self {
        self.youtube = Some(youtube);
        self
    }

    /// Заменяет музыкальный backend (например, встроенным Spotify).
    pub fn with_music(mut self, music: Arc<dyn MusicControl>) -> Self {
        self.music = Some(music);
        self
    }

    /// Приглушает музыку на время речи. Возвращённый guard вернёт громкость
    /// при разрушении, в том числе при ошибке или отмене хода.
    pub fn duck_music(&self) -> MusicDuck {
        if let Some(music) = &self.music {
            music.duck();
        }
        MusicDuck(self.music.clone())
    }

    pub fn with_memory(mut self, memory: SharedMemory) -> Self {
        self.memory = Some(memory);
        self
    }

    /// Переключение источника. Spotify и YouTube не должны звучать
    /// одновременно: в микрофон попадают оба, STT слышит кашу, а экран
    /// прыгает между двумя позициями.
    fn switch_to_music(&self) {
        if let Some(yt) = &self.youtube {
            if yt.is_playing() {
                let _ = yt.stop();
            }
        }
    }

    fn switch_to_video(&self) {
        if let Some(music) = &self.music {
            if music.is_playing().unwrap_or(false) {
                let _ = music.pause();
            }
        }
    }

    /// Отправляет на экран факт начала воспроизведения. Точную позицию
    /// присылает плеер отдельными событиями; здесь важен сам факт и название.
    fn media_event(&self, text: String) {
        let title = text
            .trim_start_matches("Включаю:")
            .trim_start_matches("Продолжаю:")
            .trim()
            .chars()
            .take(80)
            .collect::<String>();
        let _ = self.ui.send(UiEvent::Playback {
            playing: true,
            title,
            position_s: 0,
            duration_s: 0,
        });
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
            "alarm",
            "volume",
            "music",
            "youtube",
            "emotion",
            "web_search",
            "web_fetch",
            "weather",
            "sugar",
            "remember",
            "forget",
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
            "youtube" => self.youtube.is_some(),
            "web_search" => self.web.has_search_api(),
            "weather" => self.weather.is_some(),
            "sugar" => self.nightscout.is_some(),
            "remember" | "forget" => self.memory.is_some(),
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
                "Таймеры: add (seconds 1..86400), list, cancel по id. Таймеры переживают перезапуск и не зависят от разговора.",
                obj(
                    json!({
                        "action": {"type":"string","enum":["add","list","cancel"]},
                        "seconds": {"type":"integer","minimum":1,"maximum":86400},
                        "label": {"type":"string","maxLength":80},
                        "id": {"type":"integer","minimum":1}
                    }),
                    json!(["action"]),
                ),
            ),
            "alarm" => ToolDefinition::new(
                "alarm",
                "Будильники: set на ближайшее HH:MM, repeat — дни недели 1..7/пн..вс/daily/будни/выходные; cancel, list, snooze и dismiss для звонящего.",
                obj(
                    json!({
                        "action": {"type":"string","enum":["set","cancel","list","snooze","dismiss"]},
                        "time": {"type":"string","pattern":"^([01]?[0-9]|2[0-3]):[0-5][0-9]$"},
                        "label": {"type":"string","maxLength":80},
                        "id": {"type":"integer","minimum":1},
                        "repeat": {"type":"array","items":{"type":"string","maxLength":24},"maxItems":7},
                        "minutes": {"type":"integer","minimum":1,"maximum":1440}
                    }),
                    json!(["action"]),
                ),
            ),
            "emotion" => ToolDefinition::new(
                "emotion",
                "Показать эмоцию на часах и кольце коротким звуком",
                obj(
                    json!({"name":{"type":"string","enum":["happy","sad","surprised","angry","calm","greeting","neutral"]}}),
                    json!(["name"]),
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
                "Spotify: play (поиск по названию), pause, resume, next, previous, status, seek (секунды, ±), seek_to (секунды), queue, sleep (минуты), save (пауза), continue (сохранённая позиция)",
                obj(
                    json!({
                        "action":{"type":"string","enum":["play","pause","resume","next","previous","status","seek","seek_to","queue","sleep","save","continue"]},
                        "query":{"type":"string","minLength":1,"maxLength":200},
                        "seconds":{"type":"integer","minimum":-86400,"maximum":86400},
                        "minutes":{"type":"integer","minimum":1,"maximum":1440}
                    }),
                    json!(["action"]),
                ),
            ),
            "youtube" => ToolDefinition::new(
                "youtube",
                "Звук YouTube. play по ссылке, id или названию; search (limit 1..50, page — токен продолжения); details (субтитры и комментарии); pause, resume, stop, status; seek (±секунды), seek_to, queue, sleep (минуты), resume (сохранённая позиция)",
                obj(
                    json!({
                        "action":{"type":"string","enum":["play","search","details","pause","resume","stop","status","seek","seek_to","queue","sleep","continue"]},
                        "query":{"type":"string","minLength":1,"maxLength":500},
                        "id":{"type":"string","minLength":1,"maxLength":100},
                        "limit":{"type":"integer","minimum":1,"maximum":50},
                        "page":{"type":"string","maxLength":4000},
                        "seconds":{"type":"integer","minimum":-86400,"maximum":86400},
                        "minutes":{"type":"integer","minimum":1,"maximum":1440}
                    }),
                    json!(["action"]),
                ),
            ),
            "web_search" => ToolDefinition::new(
                "web_search",
                "Поиск в интернете",
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
                "Текущая погода и прогноз на день. location — город или место; без него берётся настроенное место",
                obj(
                    json!({"location":{"type":"string","maxLength":120}}),
                    json!([]),
                ),
            ),
            "sugar" => ToolDefinition::new(
                "sugar",
                "Сахар (глюкоза) из Nightscout: текущее значение, тренд, изменение и диапазон за последние часы. hours — глубина истории, 1..24, по умолчанию 3",
                obj(
                    json!({"hours":{"type":"integer","minimum":1,"maximum":24}}),
                    json!([]),
                ),
            ),
            "remember" => ToolDefinition::new(
                "remember",
                "Запомнить факт о пользователе надолго: имя, город, предпочтения, договорённости. Не сохраняй пароли, ключи и другие секреты",
                obj(
                    json!({"text":{"type":"string","minLength":1,"maxLength":500}}),
                    json!(["text"]),
                ),
            ),
            "forget" => ToolDefinition::new(
                "forget",
                "Удалить из долговременной памяти факты, содержащие text. Пустой text очищает всю память",
                obj(
                    json!({"text":{"type":"string","maxLength":200}}),
                    json!([]),
                ),
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
            "web_fetch" | "web_search" | "weather" | "sugar" => {
                Duration::from_millis(self.cfg.web_timeout_ms)
            }
            "run_code" => Duration::from_millis(self.cfg.sandbox_timeout_ms),
            // Разрешение ссылки и проверка первого сегмента ходят в сеть;
            // при заблокированном googlevideo это занимает несколько секунд.
            "youtube" => Duration::from_millis(self.cfg.timeout_ms.max(20_000)),
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
                let action = args.get("action").and_then(Value::as_str).unwrap_or("add");
                match action {
                    "add" => {
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
                    "list" => {
                        let all = self.timers.list().await;
                        if all.is_empty() {
                            return Ok(ToolResult::text("Таймеров нет."));
                        }
                        let mut out = String::new();
                        for t in all {
                            out.push_str(&format!(
                                "{}: «{}», осталось {} с (id {})\n",
                                t.id, t.label, t.remaining_s, t.id
                            ));
                        }
                        Ok(ToolResult::text(out.trim_end().to_owned()))
                    }
                    "cancel" => {
                        let id = args
                            .get("id")
                            .and_then(Value::as_u64)
                            .ok_or_else(|| anyhow::anyhow!("timer: для cancel нужен id"))?;
                        if self.timers.cancel(id).await {
                            Ok(ToolResult::text(format!("Таймер {id} отменён.")))
                        } else {
                            Ok(ToolResult::text(format!("Таймера {id} нет.")))
                        }
                    }
                    _ => bail!("timer: неизвестное действие {action}"),
                }
            }
            "alarm" => {
                let action = args.get("action").and_then(Value::as_str).unwrap_or("set");
                match action {
                    "cancel" | "snooze" | "dismiss" => {
                        let id = args
                            .get("id")
                            .and_then(Value::as_u64)
                            .ok_or_else(|| anyhow::anyhow!("alarm: для {action} нужен id"))?;
                        match action {
                            "cancel" => {
                                if self.alarms.cancel(id).await {
                                    Ok(ToolResult::text(format!("Будильник {id} отменён.")))
                                } else {
                                    Ok(ToolResult::text(format!("Будильника {id} нет.")))
                                }
                            }
                            "snooze" => {
                                let minutes = args
                                    .get("minutes")
                                    .and_then(Value::as_u64)
                                    .unwrap_or_else(|| self.alarms.snooze_minutes());
                                if self.alarms.snooze(id, minutes).await {
                                    Ok(ToolResult::text(format!(
                                        "Будильник {id} отложен на {minutes} мин."
                                    )))
                                } else {
                                    Ok(ToolResult::text(format!("Будильника {id} нет.")))
                                }
                            }
                            _ => {
                                if self.alarms.dismiss(id).await {
                                    Ok(ToolResult::text(format!("Будильник {id} выключен.")))
                                } else {
                                    Ok(ToolResult::text(format!("Будильника {id} нет.")))
                                }
                            }
                        }
                    }
                    "list" => {
                        let all = self.alarms.list().await;
                        if all.is_empty() {
                            return Ok(ToolResult::text("Будильников нет."));
                        }
                        let mut out = String::new();
                        for a in all {
                            out.push_str(&format!(
                                "{:02}:{:02} «{}» (id {}, {}, {})\n",
                                a.hour,
                                a.minute,
                                a.label,
                                a.id,
                                crate::timers::weekdays_label(&a.repeat),
                                if a.ringing {
                                    "звонит"
                                } else {
                                    "ожидает"
                                }
                            ));
                        }
                        Ok(ToolResult::text(out.trim_end().to_owned()))
                    }
                    "set" => {
                        let text = args
                            .get("time")
                            .and_then(Value::as_str)
                            .ok_or_else(|| anyhow::anyhow!("alarm: нет time"))?;
                        let (hour, minute) = parse_hhmm(text)?;
                        let label = args
                            .get("label")
                            .and_then(Value::as_str)
                            .unwrap_or("будильник")
                            .chars()
                            .take(80)
                            .collect::<String>();
                        let repeat: Vec<String> = args
                            .get("repeat")
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_owned)
                                    .collect()
                            })
                            .unwrap_or_default();
                        let repeat = crate::timers::parse_weekdays(&repeat);
                        let now = Local::now();
                        let mut target = now
                            .date_naive()
                            .and_hms_opt(hour, minute, 0)
                            .ok_or_else(|| anyhow::anyhow!("alarm: неверное время"))?;
                        if target <= now.naive_local() {
                            target += chrono::Duration::days(1);
                        }
                        let seconds = (target - now.naive_local()).num_seconds().max(1) as u64;
                        let id = self
                            .alarms
                            .add(seconds, hour, minute, label.clone(), repeat.clone())
                            .await;
                        let repeat_text = crate::timers::weekdays_label(&repeat);
                        Ok(ToolResult::text(format!(
                            "Будильник «{label}» на {hour:02}:{minute:02} ({repeat_text}) поставлен (id {id})."
                        )))
                    }
                    _ => bail!("alarm: неизвестное действие {action}"),
                }
            }
            "emotion" => {
                let name = args
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("neutral");
                let emotion = Emotion::parse(name)
                    .ok_or_else(|| anyhow::anyhow!("emotion: неизвестная эмоция {name}"))?;
                self.chime.play(ChimeSound::Emotion(emotion))?;
                let _ = self.ui.send(UiEvent::Emotion(emotion));
                Ok(ToolResult::text(format!(
                    "Эмоция «{}» показана.",
                    emotion.as_str()
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
                if action != "get" {
                    let _ = self.ui.send(UiEvent::Volume(status.percent));
                }
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
                    "play" => {
                        self.switch_to_music();
                        match args.get("query").and_then(Value::as_str) {
                            Some(q) if !q.trim().is_empty() => {
                                let text = music.play_search(q.trim()).await?;
                                self.media_event(text.clone());
                                Ok(ToolResult::text(text))
                            }
                            _ => {
                                let text = match music.resume_saved().await {
                                    Ok(text) => text,
                                    Err(_) => {
                                        music.resume_checked().await?;
                                        "Продолжаю.".to_owned()
                                    }
                                };
                                self.media_event(text.clone());
                                Ok(ToolResult::text(text))
                            }
                        }
                    }
                    "resume" => {
                        self.switch_to_music();
                        music.resume_checked().await?;
                        Ok(ToolResult::text("Продолжаю."))
                    }
                    "pause" => {
                        music.pause()?;
                        Ok(ToolResult::text("Пауза."))
                    }
                    "next" => {
                        music.next()?;
                        Ok(ToolResult::text("Следующий трек."))
                    }
                    "previous" => {
                        music.previous()?;
                        Ok(ToolResult::text("Предыдущий трек."))
                    }
                    "status" => Ok(ToolResult::text(music.status()?)),
                    "seek" => {
                        let seconds = args
                            .get("seconds")
                            .and_then(Value::as_i64)
                            .ok_or_else(|| anyhow::anyhow!("music: для seek нужны seconds"))?;
                        music.seek_by(seconds * 1000)?;
                        Ok(ToolResult::text(format!("Перемотал на {seconds} с.")))
                    }
                    "seek_to" => {
                        let seconds = args
                            .get("seconds")
                            .and_then(Value::as_u64)
                            .ok_or_else(|| anyhow::anyhow!("music: для seek_to нужны seconds"))?;
                        music.seek_to(seconds * 1000)?;
                        Ok(ToolResult::text(format!("Позиция {seconds} с.")))
                    }
                    "queue" => {
                        let query = args
                            .get("query")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|q| !q.is_empty())
                            .ok_or_else(|| anyhow::anyhow!("music: для queue нужен query"))?;
                        Ok(ToolResult::text(music.queue(query).await?))
                    }
                    "sleep" => {
                        let minutes = args
                            .get("minutes")
                            .and_then(Value::as_u64)
                            .ok_or_else(|| anyhow::anyhow!("music: для sleep нужны minutes"))?;
                        music.sleep_timer(minutes)?;
                        Ok(ToolResult::text(format!(
                            "Таймер сна на {minutes} мин поставлен."
                        )))
                    }
                    "save" => {
                        music.pause()?;
                        Ok(ToolResult::text("Позиция сохранена, пауза."))
                    }
                    "continue" => {
                        self.switch_to_music();
                        let text = music.resume_saved().await?;
                        self.media_event(text.clone());
                        Ok(ToolResult::text(text))
                    }
                    _ => bail!("music: неизвестное действие {action}"),
                }
            }
            "youtube" => {
                let yt = self
                    .youtube
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("youtube не настроен"))?;
                let action = args
                    .get("action")
                    .and_then(Value::as_str)
                    .unwrap_or("status");
                match action {
                    "play" => {
                        let query = args
                            .get("query")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|q| !q.is_empty())
                            .ok_or_else(|| anyhow::anyhow!("youtube: для play нужен query"))?;
                        self.switch_to_video();
                        let text = yt.play(query).await?;
                        self.media_event(text.clone());
                        Ok(ToolResult::text(text))
                    }
                    "search" => {
                        let query = args
                            .get("query")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|q| !q.is_empty())
                            .ok_or_else(|| anyhow::anyhow!("youtube: для search нужен query"))?;
                        let limit = args
                            .get("limit")
                            .and_then(Value::as_u64)
                            .unwrap_or(20)
                            .clamp(1, 50) as usize;
                        let page = args.get("page").and_then(Value::as_str);
                        let found = yt.search(query, limit, page).await?;
                        if found.hits.is_empty() {
                            return Ok(ToolResult::text("Ничего не найдено."));
                        }
                        let mut out = format!(
                            "Найдено{}:\n",
                            if found.estimated_results > 0 {
                                format!(" примерно {} результатов", found.estimated_results)
                            } else {
                                String::new()
                            }
                        );
                        for (i, h) in found.hits.iter().enumerate() {
                            out.push_str(&format!(
                                "{}. {} [{}] — канал {}, {}, {}; id {}\n",
                                i + 1,
                                h.title,
                                if h.live {
                                    "прямой эфир".to_owned()
                                } else {
                                    format_duration(h.duration_s)
                                },
                                h.channel,
                                if h.published.is_empty() {
                                    "дата неизвестна".to_owned()
                                } else {
                                    h.published.clone()
                                },
                                if h.views.is_empty() {
                                    String::new()
                                } else {
                                    h.views.clone()
                                },
                                h.id
                            ));
                            if !h.description.is_empty() {
                                out.push_str(&format!("   {}\n", h.description));
                            }
                        }
                        if !found.continuation.is_empty() {
                            out.push_str(&format!(
                                "Следующая страница: page=\"{}\"\n",
                                found.continuation
                            ));
                        }
                        Ok(ToolResult::text(out))
                    }
                    "details" => {
                        let id = args
                            .get("id")
                            .or_else(|| args.get("query"))
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|q| !q.is_empty())
                            .ok_or_else(|| anyhow::anyhow!("youtube: для details нужен id"))?;
                        let d = yt.details(id).await?;
                        let mut out = format!(
                            "{} — {}, канал {}, {}; id {}\n",
                            d.hit.title,
                            format_duration(d.hit.duration_s),
                            d.hit.channel,
                            d.hit.published,
                            d.hit.id
                        );
                        if !d.hit.description.is_empty() {
                            out.push_str(&format!("Описание: {}\n", d.hit.description));
                        }
                        if !d.subtitles.is_empty() {
                            out.push_str(&format!("Субтитры: {}\n", d.subtitles));
                        }
                        if !d.comments.is_empty() {
                            out.push_str(&format!("Комментарии: {}\n", d.comments));
                        }
                        Ok(ToolResult::text(out.trim_end().to_owned()))
                    }
                    "pause" => {
                        yt.pause()?;
                        Ok(ToolResult::text("Пауза."))
                    }
                    "resume" => {
                        self.switch_to_video();
                        yt.resume()?;
                        Ok(ToolResult::text("Продолжаю."))
                    }
                    "stop" => {
                        yt.stop()?;
                        let _ = self.ui.send(UiEvent::PlaybackStopped);
                        Ok(ToolResult::text("Остановлено."))
                    }
                    "status" => Ok(ToolResult::text(yt.status()?)),
                    "seek" => {
                        let seconds = args
                            .get("seconds")
                            .and_then(Value::as_i64)
                            .ok_or_else(|| anyhow::anyhow!("youtube: для seek нужны seconds"))?;
                        yt.seek_by(seconds * 1000)?;
                        Ok(ToolResult::text(format!("Перемотал на {seconds} с.")))
                    }
                    "seek_to" => {
                        let seconds = args
                            .get("seconds")
                            .and_then(Value::as_u64)
                            .ok_or_else(|| anyhow::anyhow!("youtube: для seek_to нужны seconds"))?;
                        yt.seek_to(seconds * 1000)?;
                        Ok(ToolResult::text(format!("Позиция {seconds} с.")))
                    }
                    "queue" => {
                        let query = args
                            .get("query")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|q| !q.is_empty())
                            .ok_or_else(|| anyhow::anyhow!("youtube: для queue нужен query"))?;
                        Ok(ToolResult::text(yt.queue(query).await?))
                    }
                    "sleep" => {
                        let minutes = args
                            .get("minutes")
                            .and_then(Value::as_u64)
                            .ok_or_else(|| anyhow::anyhow!("youtube: для sleep нужны minutes"))?;
                        yt.sleep_timer(minutes)?;
                        Ok(ToolResult::text(format!(
                            "Таймер сна на {minutes} мин поставлен."
                        )))
                    }
                    "continue" => {
                        self.switch_to_video();
                        let text = yt.resume_saved().await?;
                        self.media_event(text.clone());
                        Ok(ToolResult::text(text))
                    }
                    _ => bail!("youtube: неизвестное действие {action}"),
                }
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
                let hits = self.web.search(query).await?;
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
                let explicit = args
                    .get("location")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !is_generic_place(l));
                let remembered = self
                    .memory
                    .as_ref()
                    .and_then(|m| m.read().unwrap_or_else(|e| e.into_inner()).city());
                let location = explicit.map(str::to_owned).or(remembered);
                if location.is_none() && !w.has_default_location() {
                    return Ok(ToolResult::text(
                        "Я пока не знаю город. Спросите один раз: «В каком городе вы живёте?» \
                         Получив ответ, сохраните его инструментом remember как «Город: …» \
                         и сразу повторите запрос погоды.",
                    ));
                }
                let now = w.current_data_at(location.as_deref()).await?;
                if let Some(city) = explicit {
                    if let Some(memory) = &self.memory {
                        let mut memory = memory.write().unwrap_or_else(|e| e.into_inner());
                        if let Err(e) = memory.set_city(city) {
                            tracing::warn!("погода: запомнить город: {e}");
                        }
                    }
                }
                let _ = self.ui.send(UiEvent::Weather {
                    temp_c: now.temp_c,
                    feels_like: now.feels_like,
                    code: now.code,
                });
                Ok(ToolResult::text(now.summary))
            }
            "sugar" => {
                let ns = self
                    .nightscout
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Nightscout не настроен"))?;
                let hours = args.get("hours").and_then(Value::as_u64);
                let now = ns.sugar_data(hours).await?;
                let _ = self.ui.send(UiEvent::Glucose {
                    mmol: now.mmol,
                    delta: now.delta,
                    trend: now.trend.clone(),
                });
                Ok(ToolResult::text(now.text))
            }
            "remember" => {
                let memory = self
                    .memory
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("память не настроена"))?;
                let text = args.get("text").and_then(Value::as_str).unwrap_or("");
                let added = {
                    let mut memory = memory.write().unwrap_or_else(|e| e.into_inner());
                    if let Some(city) = petrovich_core::memory::extract_city(text) {
                        memory.set_city(&city)?
                    } else {
                        memory.add(text)?
                    }
                };
                Ok(ToolResult::text(if added {
                    "Запомнил."
                } else {
                    "Уже помню."
                }))
            }
            "forget" => {
                let memory = self
                    .memory
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("память не настроена"))?;
                let text = args.get("text").and_then(Value::as_str).unwrap_or("");
                let removed = {
                    let mut memory = memory.write().unwrap_or_else(|e| e.into_inner());
                    memory.forget(text)?
                };
                Ok(ToolResult::text(format!("Удалено записей: {removed}.")))
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

fn is_generic_place(text: &str) -> bool {
    crate::weather::is_generic_place(text)
}

fn format_duration(seconds: u64) -> String {
    let h = seconds / 3600;
    let m = (seconds % 3600) / 60;
    let s = seconds % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn parse_hhmm(text: &str) -> Result<(u32, u32)> {
    let t = NaiveTime::parse_from_str(text.trim(), "%H:%M")
        .map_err(|_| anyhow::anyhow!("alarm: ожидается HH:MM"))?;
    Ok((t.hour(), t.minute()))
}

/// Возвращает музыке исходную громкость при выходе из области речи.
pub struct MusicDuck(Option<Arc<dyn MusicControl>>);

impl Drop for MusicDuck {
    fn drop(&mut self) {
        if let Some(music) = &self.0 {
            music.unduck();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use petrovich_core::memory::Memory;
    use petrovich_core::ports::VolumeStatus;
    use std::sync::RwLock;
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

    struct EmptyMusic;
    impl MusicControl for EmptyMusic {
        fn play(&self) -> petrovich_core::Result<()> {
            Err(petrovich_core::Error::Unsupported(
                "Spotify: нет выбранного трека".into(),
            ))
        }
        fn pause(&self) -> petrovich_core::Result<()> {
            Ok(())
        }
        fn resume(&self) -> petrovich_core::Result<()> {
            self.play()
        }
        fn next(&self) -> petrovich_core::Result<()> {
            Ok(())
        }
        fn previous(&self) -> petrovich_core::Result<()> {
            Ok(())
        }
        fn status(&self) -> petrovich_core::Result<String> {
            Ok("Spotify: ничего не играет".into())
        }
    }

    struct FakeChime;
    impl Chime for FakeChime {
        fn play(&self, _sound: ChimeSound) -> petrovich_core::Result<()> {
            Ok(())
        }
    }

    struct FakeYouTube;
    #[async_trait::async_trait]
    impl YouTubeControl for FakeYouTube {
        async fn play(&self, query: &str) -> petrovich_core::Result<String> {
            Ok(format!("Включаю: {query}"))
        }
        fn pause(&self) -> petrovich_core::Result<()> {
            Ok(())
        }
        fn resume(&self) -> petrovich_core::Result<()> {
            Ok(())
        }
        fn stop(&self) -> petrovich_core::Result<()> {
            Ok(())
        }
        fn status(&self) -> petrovich_core::Result<String> {
            Ok("Играет: тест".into())
        }
    }

    fn ctx_ui(enabled: &[&str]) -> (ToolContext, mpsc::UnboundedReceiver<UiEvent>) {
        ctx_ui_with_music(enabled, Arc::new(FakeMusic))
    }

    fn ctx_ui_with_music(
        enabled: &[&str],
        music: Arc<dyn MusicControl>,
    ) -> (ToolContext, mpsc::UnboundedReceiver<UiEvent>) {
        let (ui, ui_rx) = mpsc::unbounded_channel();
        let (ann, _) = mpsc::channel(4);
        let (alarm_ann, _) = mpsc::channel(4);
        let cfg = ToolsConfig {
            enabled: enabled.iter().map(|s| s.to_string()).collect(),
            ..ToolsConfig::default()
        };
        let ctx = ToolContext::new(
            Arc::new(FakeVolume),
            Some(music),
            TimerManager::new(ui.clone(), ann),
            AlarmManager::new(ui.clone(), alarm_ann),
            Web::new(5000, 100_000, true).unwrap(),
            Arc::new(FakeChime),
            ui,
            cfg,
            5,
        )
        .with_youtube(Arc::new(FakeYouTube));
        (ctx, ui_rx)
    }

    fn ctx(enabled: &[&str]) -> ToolContext {
        ctx_ui(enabled).0
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
    async fn music_play_without_query_does_not_lie() {
        let (c, _) = ctx_ui_with_music(&["music"], Arc::new(EmptyMusic));
        let err = c
            .execute("music", r#"{"action":"play"}"#)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("нет выбранного трека"), "{err}");
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
    async fn youtube_tool_controls_player() {
        let c = ctx(&["youtube"]);
        let r = c
            .execute("youtube", r#"{"action":"play","query":"подкаст"}"#)
            .await
            .unwrap();
        assert!(r.content.contains("подкаст"), "{}", r.content);
        assert_eq!(
            c.execute("youtube", r#"{"action":"status"}"#)
                .await
                .unwrap()
                .content,
            "Играет: тест"
        );
        assert_eq!(
            c.execute("youtube", r#"{"action":"stop"}"#)
                .await
                .unwrap()
                .content,
            "Остановлено."
        );
        assert!(c.execute("youtube", r#"{"action":"play"}"#).await.is_err());
    }

    #[tokio::test]
    async fn alarm_and_emotion_tools_work() {
        let c = ctx(&["alarm", "emotion"]);
        let r = c
            .execute("alarm", r#"{"time":"07:30","label":"подъём"}"#)
            .await
            .unwrap();
        assert!(r.content.contains("07:30"), "{}", r.content);
        assert_eq!(c.alarms().await, 1);
        let l = c.execute("alarm", r#"{"action":"list"}"#).await.unwrap();
        assert!(l.content.contains("07:30"), "{}", l.content);
        let x = c
            .execute("alarm", r#"{"action":"cancel","id":1}"#)
            .await
            .unwrap();
        assert!(x.content.contains("отменён"));
        assert_eq!(c.alarms().await, 0);
        let e = c.execute("emotion", r#"{"name":"happy"}"#).await.unwrap();
        assert!(e.content.contains("happy"));
        assert!(c.execute("alarm", r#"{"time":"25:00"}"#).await.is_err());
    }

    #[tokio::test]
    async fn volume_tool_emits_ui_event() {
        let (c, mut ui) = ctx_ui(&["volume", "emotion"]);
        let v = c
            .execute("volume", r#"{"action":"set","value":55}"#)
            .await
            .unwrap();
        assert!(v.content.contains("55%"));
        assert!(matches!(ui.recv().await, Some(UiEvent::Volume(55))));
        c.execute("emotion", r#"{"name":"calm"}"#).await.unwrap();
        assert!(matches!(
            ui.recv().await,
            Some(UiEvent::Emotion(Emotion::Calm))
        ));
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
    async fn memory_tools_persist_facts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.json");
        let mem = Arc::new(RwLock::new(Memory::load(&path, 10, 1000, 500)));
        let c = ctx(&["remember", "forget"]).with_memory(mem);
        assert!(c
            .definitions()
            .iter()
            .any(|d| d.function.name == "remember"));
        assert_eq!(
            c.execute("remember", r#"{"text":"Любит зелёный чай"}"#)
                .await
                .unwrap()
                .content,
            "Запомнил."
        );
        assert_eq!(
            c.execute("remember", r#"{"text":"любит зелёный чай"}"#)
                .await
                .unwrap()
                .content,
            "Уже помню."
        );
        let reloaded = Memory::load(&path, 10, 1000, 500);
        assert_eq!(reloaded.facts(), &["Любит зелёный чай".to_owned()]);
        assert_eq!(
            c.execute("forget", r#"{"text":"чай"}"#)
                .await
                .unwrap()
                .content,
            "Удалено записей: 1."
        );
        assert!(Memory::load(&path, 10, 1000, 500).is_empty());
    }

    #[tokio::test]
    async fn memory_tools_require_memory() {
        let c = ctx(&["remember", "forget"]);
        assert!(c
            .definitions()
            .iter()
            .all(|d| d.function.name != "remember"));
        assert!(c.execute("remember", r#"{"text":"факт"}"#).await.is_err());
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
    async fn sugar_tool_is_available_when_configured() {
        let now = chrono::Utc::now().timestamp_millis();
        let body = format!(
            r#"[{{"type":"sgv","sgv":136,"date":{},"direction":"Flat"}},
                {{"type":"sgv","sgv":135,"date":{},"direction":"Flat"}}]"#,
            now - 20 * 60_000,
            now - 5 * 60_000
        );
        let (url, handle) = crate::testutil::serve_once(200, "application/json", &body).await;
        let ns = crate::nightscout::Nightscout::new(url, "", 5000, true).unwrap();
        let c = ctx(&["sugar"]).with_nightscout(ns);
        let r = c.execute("sugar", "{}").await.unwrap();
        assert!(r.content.contains("7.5 ммоль/л"), "{}", r.content);
        assert!(r.content.contains("стабильный"), "{}", r.content);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn sugar_requires_nightscout() {
        let c = ctx(&["sugar"]);
        assert!(c.definitions().iter().all(|d| d.function.name != "sugar"));
        assert!(c.execute("sugar", "{}").await.is_err());
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
        async fn alarms(&self) -> usize {
            self.alarms.count().await
        }
    }
}
