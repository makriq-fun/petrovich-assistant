//! Порты между инструментами и оборудованием. Реализации живут в
//! `petrovich-audio`/`petrovich-hw`, поэтому `petrovich-tools` тестируется без колонки.

use crate::errors::Result;
use crate::events::Emotion;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeStatus {
    pub percent: i32,
    pub muted: bool,
}

pub trait VolumeControl: Send + Sync {
    fn get(&self) -> Result<VolumeStatus>;
    fn set_percent(&self, percent: i32) -> Result<VolumeStatus>;
    fn step(&self, delta: i32) -> Result<VolumeStatus>;
}

pub trait MusicControl: Send + Sync {
    fn play(&self) -> Result<()>;
    /// Включить конкретный запрос (исполнитель, трек, плейлист). Реализации
    /// без поиска могут просто продолжить текущее воспроизведение.
    fn play_query(&self, _query: &str) -> Result<()> {
        self.play()
    }
    fn pause(&self) -> Result<()>;
    fn resume(&self) -> Result<()>;
    fn next(&self) -> Result<()>;
    fn previous(&self) -> Result<()>;
    fn status(&self) -> Result<String>;
    /// Играет ли музыка прямо сейчас (не пауза). Нужно глобальной «паузе»,
    /// чтобы не останавливать ассистента, когда на переднем плане музыка.
    fn is_playing(&self) -> Result<bool> {
        Ok(false)
    }
}

/// Звук YouTube: только аудиодорожка. Реализация живёт в демоне, потому что
/// ей нужны сеть и ALSA; инструменты проверяются без колонки.
#[async_trait::async_trait]
pub trait YouTubeControl: Send + Sync {
    /// Найти и включить видео по ссылке, id или поисковому запросу.
    /// Возвращает короткое описание того, что включено.
    async fn play(&self, query: &str) -> Result<String>;
    fn pause(&self) -> Result<()>;
    fn resume(&self) -> Result<()>;
    fn stop(&self) -> Result<()>;
    fn status(&self) -> Result<String>;
}

/// Устройство вывода для коротких служебных звуков (сигнал активации,
/// таймер). Полноценное воспроизведение TTS идёт через аудиопоток демона.
pub trait Chime: Send + Sync {
    fn play(&self, name: ChimeSound) -> Result<()>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChimeSound {
    /// Активация: помощник начал слушать.
    Listening,
    /// Помощник перестал слушать по тайм-ауту.
    ListeningEnd,
    /// Конец фразы: помощник услышал и думает.
    Heard,
    /// Ответ закончен.
    Done,
    /// Диалог остановлен.
    Stopped,
    /// История диалога очищена.
    Cleared,
    /// Сработал таймер.
    Timer,
    /// Сработал будильник.
    Alarm,
    /// Короткая музыкальная фраза эмоции.
    Emotion(Emotion),
    /// Ошибка.
    Error,
}
