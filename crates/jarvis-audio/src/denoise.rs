//! Простой адаптивный шумовой затвор для fallback-тракта, когда VQE
//! прошивки недоступен. Не заменяет BNR, но убирает постоянный фон.

use crate::vad::{dbfs, FRAME_MS};

pub struct NoiseGate {
    frame: usize,
    noise_db: f32,
    gain: f32,
    attack: f32,
    release: f32,
    warmup: u32,
    margin_db: f32,
    floor_gain: f32,
}

impl NoiseGate {
    pub fn new(sample_rate: u32, margin_db: f32) -> Self {
        let frame = (sample_rate as u64 * FRAME_MS / 1000).max(1) as usize;
        let attack = 1.0 - (-1.0 / (sample_rate as f32 * 0.005)).exp();
        let release = 1.0 - (-1.0 / (sample_rate as f32 * 0.080)).exp();
        Self {
            frame,
            noise_db: -60.0,
            gain: 1.0,
            attack,
            release,
            warmup: 10,
            margin_db,
            floor_gain: 0.22,
        }
    }

    pub fn process(&mut self, samples: &[i16]) -> Vec<i16> {
        let mut out = Vec::with_capacity(samples.len());
        for chunk in samples.chunks(self.frame) {
            let db = dbfs(chunk);
            if self.warmup > 0 {
                self.noise_db = (self.noise_db * 0.7 + db * 0.3).clamp(-90.0, -20.0);
                self.warmup -= 1;
            }
            let threshold = (self.noise_db + self.margin_db).min(-18.0);
            let target = if db > threshold {
                if db > threshold + 6.0 {
                    self.noise_db = (self.noise_db * 0.999 + db * 0.001).clamp(-90.0, -20.0);
                }
                1.0
            } else {
                self.floor_gain
            };
            for &s in chunk {
                let coeff = if target > self.gain {
                    self.attack
                } else {
                    self.release
                };
                self.gain += (target - self.gain) * coeff;
                out.push((f32::from(s) * self.gain).round().clamp(-32768.0, 32767.0) as i16);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(n: usize, amp: f32, seed: &mut u32) -> Vec<i16> {
        (0..n)
            .map(|_| {
                *seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                ((((*seed >> 16) as f32 / 32768.0 - 0.5) * 2.0 * amp) * 32767.0) as i16
            })
            .collect()
    }

    fn rms(x: &[i16]) -> f32 {
        (x.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / x.len() as f64).sqrt() as f32
    }

    #[test]
    fn attenuates_steady_noise() {
        let mut seed = 9;
        let mut g = NoiseGate::new(16_000, 9.0);
        let input = noise(32_000, 0.01, &mut seed);
        let _ = g.process(&input[..16_000]);
        let out = g.process(&input[16_000..]);
        assert!(rms(&out) < rms(&input[16_000..]) * 0.5, "{}", rms(&out));
    }

    #[test]
    fn keeps_loud_speech() {
        let mut g = NoiseGate::new(16_000, 9.0);
        let tone: Vec<i16> = (0..16_000)
            .map(|i| {
                (f32::sin(2.0 * std::f32::consts::PI * 440.0 * i as f32 / 16_000.0) * 8000.0) as i16
            })
            .collect();
        let out = g.process(&tone);
        assert!(rms(&out) > 4000.0, "{}", rms(&out));
    }

    #[test]
    fn short_quiet_burst_passes_attack() {
        let mut g = NoiseGate::new(16_000, 9.0);
        let silence = vec![0i16; 8000];
        let _ = g.process(&silence);
        let mut burst = vec![0i16; 1600];
        burst[..800].fill(10_000);
        let out = g.process(&burst);
        assert!(out.iter().any(|v| v.abs() > 1000));
    }
}
