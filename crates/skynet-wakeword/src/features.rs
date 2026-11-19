//! Потоковый фронтенд: PCM 16 кГц -> эмбеддинги каждые 80 мс.

use anyhow::{Context, Result};

use crate::embed::{EmbedNet, EMB_DIM, EMB_WINDOW};
use crate::mel::{MelFront, CTX_SAMP, MEL_BINS, STEP_FRAMES, STEP_SAMP};
use crate::tensors;

pub struct Features {
    mel: MelFront,
    net: EmbedNet,
    audio: Vec<f32>,
    frame: Vec<f32>,
    nmel: usize,
}

impl Features {
    pub fn load(assets: &std::path::Path) -> Result<Self> {
        let bytes =
            std::fs::read(assets).map_err(|e| anyhow::anyhow!("{}: {e}", assets.display()))?;
        let tensors = tensors::read(&bytes).context("разбор OWW1")?;
        let filters = tensors
            .get("mel.filters")
            .ok_or_else(|| anyhow::anyhow!("в OWW1 нет mel.filters"))?;
        let mel = MelFront::new(&filters.data).context("мел-фильтры")?;
        let net = EmbedNet::new(&tensors).context("эмбеддинги")?;
        Ok(Self {
            mel,
            net,
            // Как в эталонной реализации: первые 480 сэмплов — нулевой
            // контекст, иначе первый шаг сдвинут на 30 мс.
            audio: vec![0.0; CTX_SAMP],
            frame: vec![0.0; (STEP_FRAMES + 1) * MEL_BINS],
            nmel: 0,
        })
    }

    pub fn reset(&mut self) {
        self.audio.clear();
        self.audio.resize(CTX_SAMP, 0.0);
        self.nmel = 0;
        self.net.reset();
    }

    /// Подаёт PCM; возвращает завершённые эмбеддинги (по 96).
    pub fn push(&mut self, samples: &[i16]) -> Vec<Vec<f32>> {
        let mut out = Vec::new();
        for &s in samples {
            self.audio.push(f32::from(s));
            if self.audio.len() < CTX_SAMP + STEP_SAMP {
                continue;
            }
            let n = self.mel.frames(&self.audio, &mut self.frame);
            self.audio.drain(..STEP_SAMP);
            let flat = self.net.push(&self.frame, n);
            self.nmel = (self.nmel + n).min(EMB_WINDOW);
            if self.nmel < EMB_WINDOW || flat.is_empty() {
                continue;
            }
            for e in flat.chunks_exact(EMB_DIM) {
                out.push(e.to_vec());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn assets() -> Option<std::path::PathBuf> {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/oww.bin");
        p.exists().then_some(p)
    }

    fn audio(n: usize) -> Vec<i16> {
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 16000.0).sin() * 3000.0)
            .map(|v| v as i16)
            .collect()
    }

    #[test]
    fn embedding_shape_and_split_invariance() {
        let Some(a) = assets() else {
            eprintln!("oww.bin отсутствует, тест пропущен");
            return;
        };
        let pcm = audio(16_000 * 2);
        let mut one = Features::load(&a).unwrap();
        let mut a1 = Vec::new();
        for e in one.push(&pcm) {
            a1.push(e);
        }
        let mut two = Features::load(&a).unwrap();
        let mut a2 = Vec::new();
        for chunk in pcm.chunks(333) {
            for e in two.push(chunk) {
                a2.push(e);
            }
        }
        assert!(!a1.is_empty());
        assert_eq!(a1.len(), a2.len());
        for (x, y) in a1.iter().zip(&a2) {
            assert_eq!(x.len(), EMB_DIM);
            let d = x
                .iter()
                .zip(y)
                .map(|(a, b)| (a - b).abs())
                .fold(0.0, f32::max);
            assert!(d < 1e-4, "расхождение {d}");
        }
    }

    #[test]
    fn reset_restores_clean_stream() {
        let Some(a) = assets() else {
            return;
        };
        let pcm = audio(16_000);
        let mut f = Features::load(&a).unwrap();
        let first = f.push(&pcm);
        f.reset();
        let second = f.push(&pcm);
        assert_eq!(first.len(), second.len());
    }
}
