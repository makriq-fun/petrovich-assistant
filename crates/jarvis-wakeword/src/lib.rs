//! Локальная активация по слову «Джарвис».
//!
//! Модель — JSON с компактной головой и общим feature-backbone openWakeWord.
//! Обученные на голосе владельца модели в репозиторий не входят; установщик
//! импортирует их отдельно.

pub mod detector;
pub mod embed;
pub mod features;
pub mod mel;
pub mod model;
pub mod tensors;

use std::path::{Path, PathBuf};

pub use detector::{Detection, Detector};
pub use model::WakeModel;

/// Готовый к работе детектор с параметрами из конфигурации.
pub struct WakeWord {
    detector: Detector,
    name: String,
    model_path: PathBuf,
}

impl WakeWord {
    pub fn load(
        model_path: &Path,
        assets: &Path,
        threshold: Option<f32>,
        sensitivity: f32,
        gain: u32,
        refractory_ms: u64,
    ) -> anyhow::Result<Self> {
        let model = WakeModel::load(model_path)?;
        let detector = Detector::new(&model, assets, threshold, sensitivity, gain, refractory_ms)
            .map_err(|e| anyhow::anyhow!("{}: {e:#}", assets.display()))?;
        Ok(Self {
            detector,
            name: model.name,
            model_path: model_path.to_path_buf(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    pub fn threshold(&self) -> f32 {
        self.detector.threshold()
    }

    pub fn last_score(&self) -> f32 {
        self.detector.last_score()
    }

    pub fn push(&mut self, pcm: &[i16], now_ms: u64) -> Vec<Detection> {
        self.detector.push(pcm, now_ms)
    }

    pub fn reset(&mut self) {
        self.detector.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_files_report_paths() {
        let e = WakeWord::load(
            Path::new("/nonexistent/model.json"),
            Path::new("/nonexistent/oww.bin"),
            None,
            0.0,
            32,
            2000,
        );
        let msg = match e {
            Ok(_) => panic!("ожидалась ошибка"),
            Err(e) => format!("{e:#}"),
        };
        assert!(msg.contains("model.json"));
    }
}
