//! Аудиотракт: ALSA, ресемплинг, эхоподавление, VAD и обёртка VQE прошивки.

pub mod aec;
pub mod alsa;
pub mod chime;
pub mod frontend;
pub mod resample;
pub mod vad;
pub mod volume;
pub mod vqe;

pub use chime::LocalChime;
pub use frontend::{AudioFrontend, CaptureBlock, PcmFrame};
pub use volume::AmixerVolume;
