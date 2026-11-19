//! Заменяемые провайдеры: распознавание речи, языковая модель, синтез речи.

pub mod http;
pub mod llm;
pub mod stt;
pub mod tts;

#[cfg(test)]
mod testutil;

pub use llm::OpenAiLlm;
pub use stt::{from_config as stt_from_config, Stt};
pub use tts::{from_config as tts_from_config, AudioStream, Tts};
