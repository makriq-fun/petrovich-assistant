use serde::{Deserialize, Serialize};

/// Состояние голосового конвейера. Используется индикацией и журналом.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceState {
    Idle,
    Listening,
    Thinking,
    Speaking,
    Error,
}

impl VoiceState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Listening => "listening",
            Self::Thinking => "thinking",
            Self::Speaking => "speaking",
            Self::Error => "error",
        }
    }
}

/// События для светодиодов, часов и журнала. Аудио и сеть их не блокируют.
#[derive(Clone, Debug)]
pub enum UiEvent {
    State(VoiceState),
    /// Кратковременная ошибка: подсветка красным и запись в журнал.
    Error(String),
    /// Пользователь изменил громкость (0..100).
    Volume(i32),
    /// Таймер запущен/сработал/отменён.
    Timer(TimerEvent),
    /// Аппаратный мут микрофонов.
    Muted(bool),
    /// Ответ ассистента начинается: удобно для визуализации.
    SpeechStarted,
    SpeechFinished,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimerEvent {
    Started {
        id: u64,
        seconds: u64,
        label: String,
    },
    Fired {
        id: u64,
        label: String,
    },
    Cancelled {
        id: u64,
    },
}
