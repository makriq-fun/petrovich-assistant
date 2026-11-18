//! Компактная голова модели: два скрытых слоя по 24 нейрона над 16
//! эмбеддингами. Формат JSON совместим с обученными моделями станции.

use serde::{Deserialize, Serialize};

use crate::embed::{EMB_DIM, EMB_WINDOW};

pub const CTX: usize = 16;
const H1: usize = 24;
const H2: usize = 24;
const IN2: usize = CTX * H1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Head {
    pub w1: Vec<f32>, // [EMB_DIM][H1]
    pub b1: Vec<f32>,
    pub w2: Vec<f32>, // [CTX*H1][H2]
    pub b2: Vec<f32>,
    pub w3: Vec<f32>, // [H2]
    pub b3: Vec<f32>,
}

impl Head {
    pub fn validate(&self) -> anyhow::Result<()> {
        let want = [EMB_DIM * H1, H1, IN2 * H2, H2, H2, 1];
        let got = [
            self.w1.len(),
            self.b1.len(),
            self.w2.len(),
            self.b2.len(),
            self.w3.len(),
            self.b3.len(),
        ];
        for (i, (g, w)) in got.iter().zip(want).enumerate() {
            anyhow::ensure!(g == &w, "голова: тензор {i} — {g}, нужно {w}");
        }
        anyhow::ensure!(CTX * EMB_DIM == CTX * EMB_DIM);
        let _ = EMB_WINDOW;
        Ok(())
    }

    /// `x` — 16 эмбеддингов по 96, старые первыми.
    #[allow(clippy::needless_range_loop)]
    pub fn score(&self, x: &[f32]) -> f32 {
        debug_assert_eq!(x.len(), CTX * EMB_DIM);
        let mut a1 = [0.0f32; IN2];
        for t in 0..CTX {
            let row = &x[t * EMB_DIM..(t + 1) * EMB_DIM];
            for j in 0..H1 {
                let mut v = self.b1[j];
                for i in 0..EMB_DIM {
                    v += row[i] * self.w1[i * H1 + j];
                }
                a1[t * H1 + j] = v.max(0.0);
            }
        }
        let mut a2 = [0.0f32; H2];
        for k in 0..H2 {
            let mut v = self.b2[k];
            for j in 0..IN2 {
                v += a1[j] * self.w2[j * H2 + k];
            }
            a2[k] = v.max(0.0);
        }
        let mut z = self.b3[0];
        for k in 0..H2 {
            z += a2[k] * self.w3[k];
        }
        1.0 / (1.0 + (-z).exp())
    }
}

/// Модель активации в формате станции. Лишние поля обучения игнорируются.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WakeModel {
    pub name: String,
    pub threshold: f32,
    pub head: Head,
}

impl WakeModel {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        let model: Self =
            serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        model.validate()?;
        Ok(model)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.threshold.is_finite() && (0.0..=1.0).contains(&self.threshold),
            "порог {} вне 0..1",
            self.threshold
        );
        anyhow::ensure!(!self.name.trim().is_empty(), "пустое имя модели");
        self.head.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zeros() -> Head {
        Head {
            w1: vec![0.0; EMB_DIM * H1],
            b1: vec![0.0; H1],
            w2: vec![0.0; IN2 * H2],
            b2: vec![0.0; H2],
            w3: vec![0.0; H2],
            b3: vec![-5.0],
        }
    }

    #[test]
    fn zero_head_scores_low() {
        let h = zeros();
        h.validate().unwrap();
        let s = h.score(&vec![0.0; CTX * EMB_DIM]);
        assert!(s < 0.01, "{s}");
    }

    #[test]
    fn bad_shape_is_rejected() {
        let mut h = zeros();
        h.w1.pop();
        assert!(h.validate().is_err());
    }

    #[test]
    fn model_json_accepts_training_metadata() {
        let h = zeros();
        let text = serde_json::json!({
            "name": "джарвис",
            "threshold": 0.99,
            "created": "2026-11-16",
            "recall": 0.9,
            "takes": 82,
            "head": h,
        });
        let m: WakeModel = serde_json::from_value(text).unwrap();
        m.validate().unwrap();
    }
}
