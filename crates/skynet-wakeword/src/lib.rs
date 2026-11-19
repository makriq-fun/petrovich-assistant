//! Локальная активация. Поддерживаются два сменных движка:
//! openWakeWord-модель станции (JSON + OWW1) и microWakeWord v2 (TFLite).

pub mod detector;
pub mod embed;
pub mod features;
pub mod mel;
pub mod micro;
pub mod model;
pub mod tensors;

use std::path::{Path, PathBuf};

pub use detector::{Detection, Detector};
pub use micro::{MicroWakeWord, TfliteHead};
pub use model::WakeModel;

enum Backend {
    OpenWakeWord(Box<Detector>),
    Micro(Box<MicroWakeWord>),
}

/// Готовый к работе детектор с параметрами из конфигурации.
pub struct WakeWord {
    backend: Backend,
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
        if model_path.extension().and_then(|e| e.to_str()) == Some("tflite") {
            let name = model_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("wakeword")
                .to_owned();
            let head = TfliteHead::load(model_path)
                .map_err(|e| anyhow::anyhow!("{}: {e:#}", model_path.display()))?;
            let detector = Detector::new_tflite(
                head,
                assets,
                threshold.unwrap_or(0.5),
                sensitivity,
                gain,
                refractory_ms,
            )
            .map_err(|e| anyhow::anyhow!("{}: {e:#}", assets.display()))?;
            return Ok(Self {
                name,
                backend: Backend::OpenWakeWord(Box::new(detector)),
                model_path: model_path.to_path_buf(),
            });
        }
        if micro::is_micro_manifest(model_path) {
            let micro = MicroWakeWord::load(model_path, threshold, sensitivity, refractory_ms)
                .map_err(|e| anyhow::anyhow!("{}: {e:#}", model_path.display()))?;
            return Ok(Self {
                name: micro.name().to_owned(),
                backend: Backend::Micro(Box::new(micro)),
                model_path: model_path.to_path_buf(),
            });
        }
        let model = WakeModel::load(model_path)?;
        let detector = Detector::new(&model, assets, threshold, sensitivity, gain, refractory_ms)
            .map_err(|e| anyhow::anyhow!("{}: {e:#}", assets.display()))?;
        Ok(Self {
            name: model.name,
            backend: Backend::OpenWakeWord(Box::new(detector)),
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
        match &self.backend {
            Backend::OpenWakeWord(d) => d.threshold(),
            Backend::Micro(m) => m.threshold(),
        }
    }

    pub fn last_score(&self) -> f32 {
        match &self.backend {
            Backend::OpenWakeWord(d) => d.last_score(),
            Backend::Micro(m) => m.last_score(),
        }
    }

    pub fn push(&mut self, pcm: &[i16], now_ms: u64) -> Vec<Detection> {
        match &mut self.backend {
            Backend::OpenWakeWord(d) => d.push(pcm, now_ms),
            Backend::Micro(m) => m.push(pcm, now_ms),
        }
    }

    pub fn reset(&mut self) {
        match &mut self.backend {
            Backend::OpenWakeWord(d) => d.reset(),
            Backend::Micro(m) => m.reset(),
        }
    }

    pub fn is_micro(&self) -> bool {
        matches!(self.backend, Backend::Micro(_))
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
