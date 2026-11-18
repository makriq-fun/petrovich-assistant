//! Общие типы, конфигурация и протоколы jarvisd.
//!
//! Крейт намеренно не знает ни про ALSA, ни про HTTP: его можно тестировать
//! на хосте и переиспользовать в установщике и диагностике.

pub mod chat;
pub mod config;
pub mod errors;
pub mod events;
pub mod history;
pub mod ports;
pub mod profile;
pub mod text;
pub mod wav;

pub use config::{Config, LoadedConfig, Secrets};
pub use errors::{ConfigError, Error, Result};
pub use events::{TimerEvent, UiEvent, VoiceState};

/// Версия демона и протокола конфигурации.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const CONFIG_VERSION: u32 = 1;
