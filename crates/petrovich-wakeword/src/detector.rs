//! Детектор слова: эмбеддинги -> скользящее окно 16 -> голова модели ->
//! два подряд шага выше порога -> срабатывание с рефрактерным периодом.

use std::path::Path;

use anyhow::{Context, Result};

use crate::embed::EMB_DIM;
use crate::features::Features;
use crate::micro::TfliteHead;
use crate::model::{Head, WakeModel, CTX};

#[derive(Clone, Debug, PartialEq)]
pub struct Detection {
    pub score: f32,
    pub at_ms: u64,
}

enum HeadKind {
    Json(Head),
    Tflite(TfliteHead),
}

impl HeadKind {
    fn score(&self, x: &[f32]) -> f32 {
        match self {
            Self::Json(h) => h.score(x),
            Self::Tflite(h) => h.score(x),
        }
    }
}

pub struct Detector {
    features: Features,
    head: HeadKind,
    threshold: f32,
    refractory_ms: u64,
    gain: u32,
    ring: Vec<f32>,
    have: usize,
    run: usize,
    last_hit: Option<u64>,
    last_score: f32,
    scratch: Vec<i16>,
}

impl Detector {
    pub fn new(
        model: &WakeModel,
        assets: &Path,
        threshold: Option<f32>,
        sensitivity: f32,
        gain: u32,
        refractory_ms: u64,
    ) -> Result<Self> {
        let features = Features::load(assets).context("загрузка feature-backbone")?;
        let threshold = (threshold.unwrap_or(model.threshold) + sensitivity).clamp(0.05, 0.999);
        Ok(Self {
            features,
            head: HeadKind::Json(model.head.clone()),
            threshold,
            refractory_ms,
            gain: gain.max(1),
            ring: vec![0.0; CTX * EMB_DIM],
            have: 0,
            run: 0,
            last_hit: None,
            last_score: 0.0,
            scratch: Vec::new(),
        })
    }

    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    pub fn new_tflite(
        head: TfliteHead,
        assets: &Path,
        threshold: f32,
        sensitivity: f32,
        gain: u32,
        refractory_ms: u64,
    ) -> Result<Self> {
        let features = Features::load(assets).context("загрузка feature-backbone")?;
        let threshold = (threshold + sensitivity).clamp(0.05, 0.999);
        Ok(Self {
            features,
            head: HeadKind::Tflite(head),
            threshold,
            refractory_ms,
            gain: gain.max(1),
            ring: vec![0.0; CTX * EMB_DIM],
            have: 0,
            run: 0,
            last_hit: None,
            last_score: 0.0,
            scratch: Vec::new(),
        })
    }

    pub fn last_score(&self) -> f32 {
        self.last_score
    }

    pub fn reset(&mut self) {
        self.features.reset();
        self.ring.fill(0.0);
        self.have = 0;
        self.run = 0;
        self.last_hit = None;
        self.last_score = 0.0;
    }

    pub fn push(&mut self, pcm: &[i16], now_ms: u64) -> Vec<Detection> {
        let samples: &[i16] = if self.gain == 1 {
            pcm
        } else {
            self.scratch.clear();
            self.scratch.extend(pcm.iter().map(|&s| {
                let v = i32::from(s) * self.gain as i32;
                v.clamp(-32768, 32767) as i16
            }));
            &self.scratch
        };
        let embeddings = self.features.push(samples);
        let mut out = Vec::new();
        for e in embeddings {
            self.ring.copy_within(EMB_DIM.., 0);
            self.ring[(CTX - 1) * EMB_DIM..].copy_from_slice(&e);
            if self.have < CTX {
                self.have += 1;
                continue;
            }
            let score = self.head.score(&self.ring);
            self.last_score = score;
            if score < self.threshold {
                self.run = 0;
                continue;
            }
            self.run += 1;
            if self.run < 2 {
                continue;
            }
            self.run = 0;
            let allowed = match self.last_hit {
                Some(t) => now_ms.saturating_sub(t) >= self.refractory_ms,
                None => true,
            };
            if allowed {
                self.last_hit = Some(now_ms);
                out.push(Detection {
                    score,
                    at_ms: now_ms,
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WakeModel;

    fn assets() -> Option<std::path::PathBuf> {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/oww.bin");
        p.exists().then_some(p)
    }

    fn model(b3: f32) -> WakeModel {
        WakeModel {
            name: "тест".into(),
            threshold: 0.5,
            head: Head {
                w1: vec![0.0; 96 * 24],
                b1: vec![0.0; 24],
                w2: vec![0.0; 16 * 24 * 24],
                b2: vec![0.0; 24],
                w3: vec![0.0; 24],
                b3: vec![b3],
            },
        }
    }

    fn pcm(ms: u32) -> Vec<i16> {
        (0..16_000 * ms as usize / 1000)
            .map(|i| (((i * 37) % 2000) as i32 - 1000) as i16)
            .collect()
    }

    #[test]
    fn silent_head_never_triggers() {
        let Some(a) = assets() else {
            return;
        };
        let mut d = Detector::new(&model(-10.0), &a, None, 0.0, 1, 2000).unwrap();
        let mut hits = 0;
        for i in 0..100 {
            hits += d.push(&pcm(20), i * 20).len();
        }
        assert_eq!(hits, 0);
    }

    #[test]
    fn always_high_head_triggers_and_respects_refractory() {
        let Some(a) = assets() else {
            return;
        };
        let mut d = Detector::new(&model(10.0), &a, None, 0.0, 1, 2000).unwrap();
        let mut hits = Vec::new();
        let mut t = 0u64;
        for _ in 0..200 {
            hits.extend(d.push(&pcm(20), t));
            t += 20;
        }
        assert!(!hits.is_empty());
        assert!(hits.len() <= 3, "{}", hits.len());
        for w in hits.windows(2) {
            assert!(w[1].at_ms - w[0].at_ms >= 2000);
        }
    }

    #[test]
    fn gain_is_applied_without_overflow() {
        let Some(a) = assets() else {
            return;
        };
        let mut d = Detector::new(&model(-10.0), &a, None, 0.0, 64, 0).unwrap();
        let loud = vec![30000i16; 16000];
        let _ = d.push(&loud, 0);
    }
}
