//! Таймеры и будильники: независимые задачи, переживающие разговор и
//! перезапуск демона.
//!
//! Время хранится в UNIX-секундах, а не в `Instant`: после перезапуска
//! `Instant` обнуляется. Состояние пишется атомарно в `state_dir`, права
//! 0600. Будильники умеют повторяться по дням недели, звонить до отключения
//! и откладываться.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::{Datelike, Duration as ChronoDuration, Local};
use petrovich_core::config::AlarmConfig;
use petrovich_core::events::{AlarmEvent, TimerEvent, UiEvent};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, Mutex};

#[derive(Clone, Debug)]
pub struct TimerFired {
    pub id: u64,
    pub label: String,
}

#[derive(Clone, Debug)]
pub struct TimerInfo {
    pub id: u64,
    pub label: String,
    pub remaining_s: u64,
}

struct Entry {
    label: String,
    deadline: SystemTime,
    cancel: Option<oneshot::Sender<()>>,
}

#[derive(Clone)]
pub struct TimerManager {
    inner: Arc<Mutex<HashMap<u64, Entry>>>,
    next: Arc<AtomicU64>,
    ui: mpsc::UnboundedSender<UiEvent>,
    announce: mpsc::Sender<TimerFired>,
    state: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, Default)]
struct StoredTimers {
    #[serde(default = "state_version")]
    version: u32,
    #[serde(default)]
    timers: Vec<StoredTimer>,
}

#[derive(Serialize, Deserialize)]
struct StoredTimer {
    id: u64,
    label: String,
    deadline_unix: u64,
}

fn state_version() -> u32 {
    1
}

impl TimerManager {
    pub fn new(ui: mpsc::UnboundedSender<UiEvent>, announce: mpsc::Sender<TimerFired>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            next: Arc::new(AtomicU64::new(1)),
            ui,
            announce,
            state: None,
        }
    }

    /// Включает сохранение на диск. `restore()` вызывается демоном после
    /// создания каналов.
    pub fn with_state(mut self, path: impl Into<PathBuf>) -> Self {
        self.state = Some(path.into());
        self
    }

    async fn persist(&self) {
        let Some(path) = &self.state else {
            return;
        };
        let mut timers = Vec::new();
        for (id, entry) in self.inner.lock().await.iter() {
            timers.push(StoredTimer {
                id: *id,
                label: entry.label.clone(),
                deadline_unix: entry
                    .deadline
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            });
        }
        let data = StoredTimers { version: 1, timers };
        if let Err(e) = write_state(path, &data) {
            tracing::warn!("таймеры: сохранить {}: {e:#}", path.display());
        }
    }

    /// Восстанавливает таймеры после перезапуска. Просроченные срабатывают
    /// сразу, как только демон готов принимать уведомления.
    pub async fn restore(&self) {
        let Some(path) = &self.state else {
            return;
        };
        let text = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                tracing::warn!("таймеры: чтение {}: {e}", path.display());
                return;
            }
        };
        let stored: StoredTimers = match serde_json::from_slice(&text) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("таймеры: разбор {}: {e}", path.display());
                return;
            }
        };
        let mut max_id = 0;
        for t in stored.timers {
            max_id = max_id.max(t.id);
            let deadline = UNIX_EPOCH + Duration::from_secs(t.deadline_unix);
            let remaining = deadline
                .duration_since(SystemTime::now())
                .unwrap_or(Duration::ZERO);
            let id = self.insert(t.id, t.label, deadline).await;
            self.spawn(id, remaining).await;
            if remaining.is_zero() {
                tracing::info!("таймер {id} просрочен за время простоя");
            }
        }
        self.next.fetch_max(max_id + 1, Ordering::Relaxed);
        tracing::info!("таймеры восстановлены: {} в работе", self.count().await);
    }

    async fn insert(&self, id: u64, label: String, deadline: SystemTime) -> u64 {
        let (tx, _rx) = oneshot::channel();
        self.inner.lock().await.insert(
            id,
            Entry {
                label,
                deadline,
                cancel: Some(tx),
            },
        );
        id
    }

    async fn spawn(&self, id: u64, remaining: Duration) {
        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.inner.lock().await;
            if let Some(e) = map.get_mut(&id) {
                e.cancel = Some(tx);
            } else {
                return;
            }
        }
        let inner = Arc::clone(&self.inner);
        let ui = self.ui.clone();
        let announce = self.announce.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(remaining) => {}
                _ = rx => { return; }
            }
            let removed = {
                let mut map = inner.lock().await;
                map.remove(&id)
            };
            let Some(entry) = removed else { return };
            let _ = ui.send(UiEvent::Timer(TimerEvent::Fired {
                id,
                label: entry.label.clone(),
            }));
            let _ = announce
                .send(TimerFired {
                    id,
                    label: entry.label,
                })
                .await;
            if let Some(path) = state {
                let mut timers = Vec::new();
                for (id, entry) in inner.lock().await.iter() {
                    timers.push(StoredTimer {
                        id: *id,
                        label: entry.label.clone(),
                        deadline_unix: entry
                            .deadline
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                    });
                }
                let _ = write_state(&path, &StoredTimers { version: 1, timers });
            }
        });
    }

    pub async fn add(&self, seconds: u64, label: String) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let deadline = SystemTime::now() + Duration::from_secs(seconds.max(1));
        self.insert(id, label.clone(), deadline).await;
        let _ = self.ui.send(UiEvent::Timer(TimerEvent::Started {
            id,
            seconds,
            label: label.clone(),
        }));
        self.persist().await;
        self.spawn(id, Duration::from_secs(seconds.max(1))).await;
        id
    }

    pub async fn cancel(&self, id: u64) -> bool {
        let mut map = self.inner.lock().await;
        if let Some(mut e) = map.remove(&id) {
            if let Some(tx) = e.cancel.take() {
                let _ = tx.send(());
            }
            let _ = self.ui.send(UiEvent::Timer(TimerEvent::Cancelled { id }));
            drop(map);
            self.persist().await;
            true
        } else {
            false
        }
    }

    pub async fn list(&self) -> Vec<TimerInfo> {
        let now = SystemTime::now();
        let map = self.inner.lock().await;
        let mut out: Vec<TimerInfo> = map
            .iter()
            .map(|(id, e)| TimerInfo {
                id: *id,
                label: e.label.clone(),
                remaining_s: e.deadline.duration_since(now).unwrap_or_default().as_secs(),
            })
            .collect();
        out.sort_by_key(|t| t.remaining_s);
        out
    }

    pub async fn count(&self) -> usize {
        self.inner.lock().await.len()
    }
}

#[derive(Clone, Debug)]
pub struct AlarmFired {
    pub id: u64,
    pub hour: u32,
    pub minute: u32,
    pub label: String,
}

#[derive(Clone, Debug)]
pub struct AlarmInfo {
    pub id: u64,
    pub label: String,
    pub hour: u32,
    pub minute: u32,
    pub remaining_s: u64,
    /// Дни недели 1..7 (пн..вс); пусто — разовый.
    pub repeat: Vec<u32>,
    pub ringing: bool,
}

struct AlarmEntry {
    label: String,
    hour: u32,
    minute: u32,
    /// 1..7, пн..вс; пусто — разовый.
    repeat: Vec<u32>,
    deadline: SystemTime,
    cancel: Option<oneshot::Sender<()>>,
    ringing: bool,
}

/// Будильники на конкретное местное время с повтором по дням недели.
#[derive(Clone)]
pub struct AlarmManager {
    inner: Arc<Mutex<HashMap<u64, AlarmEntry>>>,
    next: Arc<AtomicU64>,
    ui: mpsc::UnboundedSender<UiEvent>,
    announce: mpsc::Sender<AlarmFired>,
    cfg: AlarmConfig,
    state: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, Default)]
struct StoredAlarms {
    #[serde(default = "state_version")]
    version: u32,
    #[serde(default)]
    alarms: Vec<StoredAlarm>,
}

#[derive(Serialize, Deserialize)]
struct StoredAlarm {
    id: u64,
    label: String,
    hour: u32,
    minute: u32,
    #[serde(default)]
    repeat: Vec<u32>,
    deadline_unix: u64,
}

impl AlarmManager {
    pub fn new(ui: mpsc::UnboundedSender<UiEvent>, announce: mpsc::Sender<AlarmFired>) -> Self {
        Self::with_config(AlarmConfig::default(), None, ui, announce)
    }

    pub fn with_config(
        cfg: AlarmConfig,
        state: Option<PathBuf>,
        ui: mpsc::UnboundedSender<UiEvent>,
        announce: mpsc::Sender<AlarmFired>,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            next: Arc::new(AtomicU64::new(1)),
            ui,
            announce,
            cfg,
            state,
        }
    }

    pub fn snooze_minutes(&self) -> u64 {
        self.cfg.snooze_min.max(1)
    }

    async fn persist(&self) {
        let Some(path) = &self.state else {
            return;
        };
        let map = self.inner.lock().await;
        let alarms = map
            .iter()
            .map(|(id, a)| StoredAlarm {
                id: *id,
                label: a.label.clone(),
                hour: a.hour,
                minute: a.minute,
                repeat: a.repeat.clone(),
                deadline_unix: a
                    .deadline
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            })
            .collect();
        drop(map);
        let data = StoredAlarms { version: 1, alarms };
        if let Err(e) = write_state(path, &data) {
            tracing::warn!("будильники: сохранить {}: {e:#}", path.display());
        }
    }

    /// Восстанавливает будильники и досрочно срабатывает просроченные.
    pub async fn restore(&self) {
        let Some(path) = &self.state else {
            return;
        };
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                tracing::warn!("будильники: чтение {}: {e}", path.display());
                return;
            }
        };
        let stored: StoredAlarms = match serde_json::from_slice(&bytes) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("будильники: разбор {}: {e}", path.display());
                return;
            }
        };
        let mut max_id = 0;
        for a in stored.alarms {
            max_id = max_id.max(a.id);
            let deadline = UNIX_EPOCH + Duration::from_secs(a.deadline_unix);
            {
                let mut map = self.inner.lock().await;
                map.insert(
                    a.id,
                    AlarmEntry {
                        label: a.label,
                        hour: a.hour,
                        minute: a.minute,
                        repeat: a.repeat,
                        deadline,
                        cancel: None,
                        ringing: false,
                    },
                );
            }
            self.spawn(a.id).await;
        }
        self.next.fetch_max(max_id + 1, Ordering::Relaxed);
        tracing::info!("будильники восстановлены: {}", self.count().await);
    }

    /// Ставит будильник. `seconds` — до ближайшего срабатывания, `repeat` —
    /// дни недели 1..7 для повторов.
    pub async fn add(
        &self,
        seconds: u64,
        hour: u32,
        minute: u32,
        label: String,
        repeat: Vec<u32>,
    ) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let deadline = SystemTime::now() + Duration::from_secs(seconds.max(1));
        self.inner.lock().await.insert(
            id,
            AlarmEntry {
                label: label.clone(),
                hour,
                minute,
                repeat: normalize_weekdays(repeat),
                deadline,
                cancel: None,
                ringing: false,
            },
        );
        let _ = self.ui.send(UiEvent::Alarm(AlarmEvent::Started {
            id,
            hour,
            minute,
            label,
        }));
        self.persist().await;
        self.spawn(id).await;
        id
    }

    async fn spawn(&self, id: u64) {
        let (tx, mut rx) = oneshot::channel();
        {
            let mut map = self.inner.lock().await;
            let Some(entry) = map.get_mut(&id) else {
                return;
            };
            entry.cancel = Some(tx);
            let _ = entry;
        }
        let inner = Arc::clone(&self.inner);
        let ui = self.ui.clone();
        let announce = self.announce.clone();
        let cfg = self.cfg.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            loop {
                let deadline = {
                    let map = inner.lock().await;
                    match map.get(&id) {
                        Some(a) => a.deadline,
                        None => return,
                    }
                };
                let wait = deadline
                    .duration_since(SystemTime::now())
                    .unwrap_or(Duration::ZERO);
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = &mut rx => return,
                }
                // Звоним до отключения: повторяем сигнал, пока пользователь
                // не вызовет dismiss или snooze.
                let ring_start = SystemTime::now();
                loop {
                    let (label, hour, minute) = {
                        let mut map = inner.lock().await;
                        match map.get_mut(&id) {
                            Some(a) => {
                                a.ringing = true;
                                (a.label.clone(), a.hour, a.minute)
                            }
                            None => return,
                        }
                    };
                    let _ = ui.send(UiEvent::Alarm(AlarmEvent::Fired {
                        id,
                        label: label.clone(),
                    }));
                    let _ = announce
                        .try_send(AlarmFired {
                            id,
                            hour,
                            minute,
                            label,
                        })
                        .ok();
                    if cfg.max_ring_s > 0
                        && ring_start.elapsed().unwrap_or_default()
                            >= Duration::from_secs(cfg.max_ring_s)
                    {
                        break;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(cfg.repeat_s.max(1))) => {}
                        _ = &mut rx => return,
                    }
                }
                // Автоотключение: разовый будильник удаляем, повторяющийся
                // переносим на следующий выбранный день.
                let mut map = inner.lock().await;
                let Some(entry) = map.get_mut(&id) else {
                    return;
                };
                entry.ringing = false;
                if entry.repeat.is_empty() {
                    map.remove(&id);
                    drop(map);
                    if let Some(path) = state.clone() {
                        let _ = persist_alarms(&inner, &path).await;
                    }
                    return;
                }
                entry.deadline = next_weekday_deadline(entry.hour, entry.minute, &entry.repeat);
                drop(map);
                if let Some(path) = state.clone() {
                    let _ = persist_alarms(&inner, &path).await;
                }
            }
        });
    }

    pub async fn cancel(&self, id: u64) -> bool {
        let mut map = self.inner.lock().await;
        if let Some(mut e) = map.remove(&id) {
            if let Some(tx) = e.cancel.take() {
                let _ = tx.send(());
            }
            drop(map);
            self.persist().await;
            true
        } else {
            false
        }
    }

    /// Откладывает звонящий будильник на `minutes` минут.
    pub async fn snooze(&self, id: u64, minutes: u64) -> bool {
        let deadline = SystemTime::now() + Duration::from_secs(minutes.max(1) * 60);
        {
            let mut map = self.inner.lock().await;
            let Some(e) = map.get_mut(&id) else {
                return false;
            };
            if let Some(tx) = e.cancel.take() {
                let _ = tx.send(());
            }
            e.deadline = deadline;
            e.ringing = false;
        }
        self.persist().await;
        self.spawn(id).await;
        true
    }

    /// Отключает звонящий будильник. Повторяющийся остаётся на следующий
    /// выбранный день, разовый удаляется.
    pub async fn dismiss(&self, id: u64) -> bool {
        let mut repeat_next = None;
        {
            let mut map = self.inner.lock().await;
            let Some(e) = map.get_mut(&id) else {
                return false;
            };
            if let Some(tx) = e.cancel.take() {
                let _ = tx.send(());
            }
            e.ringing = false;
            if e.repeat.is_empty() {
                map.remove(&id);
            } else {
                e.deadline = next_weekday_deadline(e.hour, e.minute, &e.repeat);
                repeat_next = Some(());
            }
        }
        self.persist().await;
        if repeat_next.is_some() {
            self.spawn(id).await;
        }
        true
    }

    pub async fn list(&self) -> Vec<AlarmInfo> {
        let now = SystemTime::now();
        let map = self.inner.lock().await;
        let mut out: Vec<AlarmInfo> = map
            .iter()
            .map(|(id, e)| AlarmInfo {
                id: *id,
                label: e.label.clone(),
                hour: e.hour,
                minute: e.minute,
                remaining_s: e.deadline.duration_since(now).unwrap_or_default().as_secs(),
                repeat: e.repeat.clone(),
                ringing: e.ringing,
            })
            .collect();
        out.sort_by_key(|a| a.remaining_s);
        out
    }

    pub async fn count(&self) -> usize {
        self.inner.lock().await.len()
    }
}

async fn persist_alarms(inner: &Arc<Mutex<HashMap<u64, AlarmEntry>>>, path: &Path) -> Result<()> {
    let map = inner.lock().await;
    let alarms = map
        .iter()
        .map(|(id, a)| StoredAlarm {
            id: *id,
            label: a.label.clone(),
            hour: a.hour,
            minute: a.minute,
            repeat: a.repeat.clone(),
            deadline_unix: a
                .deadline
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        })
        .collect();
    drop(map);
    write_state(path, &StoredAlarms { version: 1, alarms })
}

fn normalize_weekdays(mut days: Vec<u32>) -> Vec<u32> {
    days.retain(|d| (1..=7).contains(d));
    days.sort_unstable();
    days.dedup();
    days
}

/// Ближайшее выбранное время. Если сегодня день уже прошёл — следующий.
fn next_weekday_deadline(hour: u32, minute: u32, repeat: &[u32]) -> SystemTime {
    let now = Local::now();
    for add in 0..=7 {
        let date = now.date_naive() + ChronoDuration::days(add);
        let weekday = date.weekday().number_from_monday();
        if !repeat.contains(&weekday) {
            continue;
        }
        let Some(naive) = date.and_hms_opt(hour.min(23), minute.min(59), 0) else {
            continue;
        };
        if add == 0 && naive <= now.naive_local() {
            continue;
        }
        if let Some(dt) = naive.and_local_timezone(Local).single() {
            return SystemTime::from(dt);
        }
    }
    // Запасной вариант: через сутки.
    SystemTime::now() + Duration::from_secs(86_400)
}

fn write_state<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| parent.display().to_string())?;
    }
    let data = serde_json::to_vec_pretty(value)?;
    let tmp = path.with_extension("json.tmp");
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options
        .open(&tmp)
        .with_context(|| tmp.display().to_string())?;
    f.write_all(&data)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| path.display().to_string())?;
    Ok(())
}

/// Разбирает дни недели из аргумента инструмента: числа 1..7, «daily»,
/// «будни», «выходные» или русские названия.
pub fn parse_weekdays(values: &[String]) -> Vec<u32> {
    let mut out = Vec::new();
    for v in values {
        let v = v.trim().to_lowercase();
        match v.as_str() {
            "daily" | "каждый день" | "ежедневно" => out.extend(1..=7),
            "weekdays" | "будни" | "по будням" => out.extend(1..=5),
            "weekend" | "выходные" | "по выходным" => out.extend([6, 7]),
            _ => {
                if let Ok(n) = v.parse::<u32>() {
                    if (1..=7).contains(&n) {
                        out.push(n);
                    }
                } else {
                    let day = match v.as_str() {
                        "пн" | "понедельник" | "mon" | "monday" => Some(1),
                        "вт" | "вторник" | "tue" | "tuesday" => Some(2),
                        "ср" | "среда" | "wed" | "wednesday" => Some(3),
                        "чт" | "четверг" | "thu" | "thursday" => Some(4),
                        "пт" | "пятница" | "fri" | "friday" => Some(5),
                        "сб" | "суббота" | "sat" | "saturday" => Some(6),
                        "вс" | "воскресенье" | "sun" | "sunday" => Some(7),
                        _ => None,
                    };
                    if let Some(d) = day {
                        out.push(d);
                    }
                }
            }
        }
    }
    normalize_weekdays(out)
}

/// Русские названия дней для ответа ассистента.
pub fn weekdays_label(days: &[u32]) -> String {
    if days.is_empty() {
        return "один раз".into();
    }
    if days.len() == 7 {
        return "каждый день".into();
    }
    if days == [1, 2, 3, 4, 5] {
        return "по будням".into();
    }
    if days == [6, 7] {
        return "по выходным".into();
    }
    let names = ["пн", "вт", "ср", "чт", "пт", "сб", "вс"];
    days.iter()
        .filter_map(|d| names.get((*d).saturating_sub(1) as usize))
        .copied()
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn alarm_fires_and_notifies() {
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let (ann_tx, mut ann_rx) = mpsc::channel(4);
        let m = AlarmManager::new(ui_tx, ann_tx);
        let id = m.add(1, 7, 30, "подъём".into(), vec![]).await;
        let fired = tokio::time::timeout(Duration::from_secs(3), ann_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fired.id, id);
        assert_eq!(fired.hour, 7);
        assert_eq!(fired.minute, 30);
        let _ = ui_rx.recv().await;
        assert!(matches!(
            ui_rx.recv().await,
            Some(UiEvent::Alarm(AlarmEvent::Fired { .. }))
        ));
        assert_eq!(m.count().await, 1, "будильник звонит до отключения");
        assert!(m.dismiss(id).await);
        assert_eq!(m.count().await, 0);
    }

    #[tokio::test]
    async fn alarm_snooze_and_dismiss() {
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let (ann_tx, _ann_rx) = mpsc::channel(4);
        let m = AlarmManager::new(ui_tx, ann_tx);
        let id = m.add(1, 7, 30, "x".into(), vec![1, 2, 3]).await;
        assert!(m.snooze(id, 1).await);
        assert!(m.dismiss(id).await);
        assert!(m.cancel(id).await);
        assert!(!m.cancel(id).await);
    }

    #[tokio::test]
    async fn timers_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("timers.json");
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let (ann_tx, mut ann_rx) = mpsc::channel(4);
        let m = TimerManager::new(ui_tx, ann_tx).with_state(&path);
        m.add(60, "чай".into()).await;
        assert!(path.exists());
        drop(m);

        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let (ann_tx, _ann_rx) = mpsc::channel(4);
        let m = TimerManager::new(ui_tx, ann_tx).with_state(&path);
        m.restore().await;
        let list = m.list().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].label, "чай");
        assert!(list[0].remaining_s <= 60);
        let _ = &mut ann_rx;
    }

    #[tokio::test]
    async fn timers_fire_and_cancel() {
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let (ann_tx, mut ann_rx) = mpsc::channel(4);
        let m = TimerManager::new(ui_tx, ann_tx);
        let id = m.add(1, "чай".into()).await;
        let fired = tokio::time::timeout(Duration::from_secs(3), ann_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fired.id, id);
        let _ = ui_rx.recv().await;
        assert!(matches!(
            ui_rx.recv().await,
            Some(UiEvent::Timer(TimerEvent::Fired { .. }))
        ));
        assert_eq!(m.count().await, 0);

        let id = m.add(1, "x".into()).await;
        assert!(m.cancel(id).await);
        assert!(!m.cancel(id).await);
    }

    #[tokio::test]
    async fn lists_remaining() {
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let (ann_tx, _ann_rx) = mpsc::channel(4);
        let m = TimerManager::new(ui_tx, ann_tx);
        m.add(10, "a".into()).await;
        m.add(20, "b".into()).await;
        let l = m.list().await;
        assert_eq!(l.len(), 2);
        assert!(l[0].remaining_s <= 10);
        assert!(l[1].remaining_s > l[0].remaining_s);
    }

    #[test]
    fn weekday_parsing_and_label() {
        assert_eq!(parse_weekdays(&["daily".into()]), vec![1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(parse_weekdays(&["пн".into(), "5".into()]), vec![1, 5]);
        assert_eq!(weekdays_label(&[1, 2, 3, 4, 5]), "по будням");
        assert_eq!(weekdays_label(&[6, 7]), "по выходным");
        assert_eq!(weekdays_label(&[]), "один раз");
    }
}
