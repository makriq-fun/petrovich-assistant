//! Порты между инструментами и оборудованием. Реализации живут в
//! `jarvis-audio`/`jarvis-hw`, поэтому `jarvis-tools` тестируется без колонки.

use crate::errors::Result;

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
    fn pause(&self) -> Result<()>;
    fn resume(&self) -> Result<()>;
    fn next(&self) -> Result<()>;
    fn previous(&self) -> Result<()>;
    fn status(&self) -> Result<String>;
}

/// Устройство вывода для коротких служебных звуков (сигнал активации,
/// таймер). Полноценное воспроизведение TTS идёт через аудиопоток демона.
pub trait Chime: Send + Sync {
    fn play(&self, name: ChimeSound) -> Result<()>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChimeSound {
    Wake,
    Done,
    Cancel,
    Timer,
    Error,
}
