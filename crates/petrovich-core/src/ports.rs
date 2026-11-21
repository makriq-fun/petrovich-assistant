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

#[async_trait::async_trait]
pub trait MusicControl: Send + Sync {
    fn play(&self) -> Result<()>;
    /// Включить конкретный запрос (исполнитель, трек, плейлист). Реализации
    /// без поиска могут просто продолжить текущее воспроизведение.
    fn play_query(&self, _query: &str) -> Result<()> {
        self.play()
    }
    /// Найти и включить контент (трек, альбом, плейлист, подкаст, выпуск).
    /// Возвращает короткое описание того, что включено.
    async fn play_search(&self, query: &str) -> Result<String> {
        self.play_query(query)?;
        Ok(format!("Включаю: {query}"))
    }
    /// Включить коллекцию «Мне нравится» (liked songs). У неё нет названия
    /// плейлиста, поэтому поиск по строке тут не подходит.
    async fn liked(&self) -> Result<String> {
        Err(crate::Error::Unsupported(
            "любимые треки не поддерживаются".into(),
        ))
    }
    fn pause(&self) -> Result<()>;
    fn resume(&self) -> Result<()>;
    /// Продолжить воспроизведение и убедиться, что оно реально пошло:
    /// «Продолжаю» нельзя произносить после молча проигнорированной команды.
    async fn resume_checked(&self) -> Result<()> {
        self.resume()
    }
    fn next(&self) -> Result<()>;
    fn previous(&self) -> Result<()>;
    fn status(&self) -> Result<String>;
    /// Играет ли музыка прямо сейчас (не пауза). Нужно глобальной «паузе»,
    /// чтобы не останавливать ассистента, когда на переднем плане музыка.
    fn is_playing(&self) -> Result<bool> {
        Ok(false)
    }
    /// Перемотка относительно текущей позиции; отрицательное значение —
    /// назад. Реализация по умолчанию недоступна.
    fn seek_by(&self, _delta_ms: i64) -> Result<()> {
        Err(crate::Error::Unsupported(
            "перемотка не поддерживается".into(),
        ))
    }
    /// Переход к абсолютной позиции.
    fn seek_to(&self, _position_ms: u64) -> Result<()> {
        Err(crate::Error::Unsupported(
            "перемотка не поддерживается".into(),
        ))
    }
    /// Добавить в очередь. Реализация по умолчанию не поддерживается.
    async fn queue(&self, _query: &str) -> Result<String> {
        Err(crate::Error::Unsupported(
            "очередь не поддерживается".into(),
        ))
    }
    /// Таймер сна: остановить воспроизведение через N минут.
    fn sleep_timer(&self, _minutes: u64) -> Result<()> {
        Err(crate::Error::Unsupported(
            "таймер сна не поддерживается".into(),
        ))
    }
    /// Продолжить последнюю сохранённую сессию/позицию.
    async fn resume_saved(&self) -> Result<String> {
        Err(crate::Error::Unsupported("сохранённой позиции нет".into()))
    }
    /// Приглушить на время ответа ассистента, не меняя паузу пользователя.
    fn duck(&self) {}
    fn unduck(&self) {}
    /// Текущая позиция и длительность, секунды (как в `UiEvent::Playback`).
    fn position(&self) -> Result<Option<(u64, u64)>> {
        Ok(None)
    }
}

/// Результат поиска YouTube. Поля необязательны: InnerTube отдаёт их не
/// для всех типов роликов.
#[derive(Clone, Debug, Default)]
pub struct YouTubeHit {
    pub id: String,
    pub title: String,
    pub channel: String,
    pub duration_s: u64,
    /// Дата публикации в том виде, в каком её отдал YouTube.
    pub published: String,
    pub description: String,
    pub live: bool,
    pub views: String,
}

#[derive(Clone, Debug, Default)]
pub struct YouTubeSearchResult {
    pub hits: Vec<YouTubeHit>,
    /// Токен следующей страницы; пусто — страниц больше нет.
    pub continuation: String,
    pub estimated_results: u64,
}

#[derive(Clone, Debug, Default)]
pub struct YouTubeDetails {
    pub hit: YouTubeHit,
    pub subtitles: String,
    pub comments: String,
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
    /// Поиск с метаданными и продолжением. `page` — токен предыдущей
    /// страницы (`continuation`), не индекс.
    async fn search(
        &self,
        _query: &str,
        _limit: usize,
        _page: Option<&str>,
    ) -> Result<YouTubeSearchResult> {
        Err(crate::Error::Unsupported(
            "поиск YouTube не поддерживается".into(),
        ))
    }
    /// Подробности ролика: описание, субтитры и комментарии, если доступны.
    async fn details(&self, _video_id: &str) -> Result<YouTubeDetails> {
        Err(crate::Error::Unsupported(
            "детали YouTube не поддерживаются".into(),
        ))
    }
    /// Включить конкретный ролик без повторного поиска.
    async fn play_video(&self, video_id: &str) -> Result<String> {
        self.play(video_id).await
    }
    fn seek_by(&self, _delta_ms: i64) -> Result<()> {
        Err(crate::Error::Unsupported(
            "перемотка не поддерживается".into(),
        ))
    }
    fn seek_to(&self, _position_ms: u64) -> Result<()> {
        Err(crate::Error::Unsupported(
            "перемотка не поддерживается".into(),
        ))
    }
    async fn queue(&self, _query: &str) -> Result<String> {
        Err(crate::Error::Unsupported(
            "очередь не поддерживается".into(),
        ))
    }
    fn sleep_timer(&self, _minutes: u64) -> Result<()> {
        Err(crate::Error::Unsupported(
            "таймер сна не поддерживается".into(),
        ))
    }
    fn is_playing(&self) -> bool {
        false
    }
    fn duck(&self) {}
    fn unduck(&self) {}
    fn position(&self) -> Option<(u64, u64)> {
        None
    }
    /// Сохранить позицию и продолжить её позже.
    async fn resume_saved(&self) -> Result<String> {
        Err(crate::Error::Unsupported("сохранённой позиции нет".into()))
    }
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
