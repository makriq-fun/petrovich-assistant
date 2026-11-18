//! Определение речи по энергии с адаптивным шумовым порогом, пре-роллом
//! и ограничением длительности. Это не нейросетевой VAD, зато предсказуем
//! на 32-битном ARM и не требует модели.

use std::collections::VecDeque;

pub const FRAME_MS: u64 = 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Utterance {
    pub samples: Vec<i16>,
    pub speech_ms: u64,
}

pub struct Vad {
    sample_rate: u32,
    frame_samples: usize,
    margin_db: f32,
    absolute_db: f32,
    min_speech_ms: u64,
    silence_ms: u64,
    max_utterance_ms: u64,
    pre_roll: VecDeque<i16>,
    pre_roll_len: usize,
    pending: Vec<i16>,
    noise_db: f32,
    above: usize,
    below: usize,
    in_speech: bool,
    speech: Vec<i16>,
    speech_frames: u64,
    voiced: bool,
    warmup: u32,
}

impl Vad {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sample_rate: u32,
        min_speech_ms: u64,
        silence_ms: u64,
        max_utterance_ms: u64,
        margin_db: f32,
        pre_roll_ms: u64,
        absolute_db: f32,
    ) -> Self {
        let frame_samples = (sample_rate as u64 * FRAME_MS / 1000) as usize;
        let pre_roll_len = (sample_rate as u64 * pre_roll_ms / 1000) as usize;
        Self {
            sample_rate,
            frame_samples: frame_samples.max(1),
            margin_db,
            absolute_db,
            min_speech_ms,
            silence_ms,
            max_utterance_ms,
            pre_roll: VecDeque::with_capacity(pre_roll_len),
            pre_roll_len,
            pending: Vec::new(),
            noise_db: -60.0,
            above: 0,
            below: 0,
            in_speech: false,
            speech: Vec::new(),
            speech_frames: 0,
            voiced: false,
            warmup: 15,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn in_speech(&self) -> bool {
        self.in_speech
    }

    pub fn voiced(&self) -> bool {
        self.voiced
    }

    pub fn noise_db(&self) -> f32 {
        self.noise_db
    }

    /// Сбрасывает текущую фразу, сохраняя оценку шума.
    pub fn reset_utterance(&mut self) {
        self.pending.clear();
        self.speech.clear();
        self.pre_roll.clear();
        self.in_speech = false;
        self.speech_frames = 0;
        self.above = 0;
        self.below = 0;
        self.voiced = false;
    }

    /// Полный сброс, например при смене источника звука.
    pub fn reset(&mut self) {
        self.reset_utterance();
        self.noise_db = -60.0;
    }

    pub fn push(&mut self, samples: &[i16]) -> Vec<Utterance> {
        self.pending.extend_from_slice(samples);
        let mut out = Vec::new();
        while self.pending.len() >= self.frame_samples {
            let frame: Vec<i16> = self.pending.drain(..self.frame_samples).collect();
            if let Some(u) = self.frame(&frame) {
                out.push(u);
            }
        }
        out
    }

    fn frame(&mut self, frame: &[i16]) -> Option<Utterance> {
        let db = dbfs(frame);
        if self.warmup > 0 {
            self.noise_db = (self.noise_db * 0.75 + db * 0.25).clamp(-90.0, -20.0);
            self.push_pre_roll(frame);
            self.warmup -= 1;
            return None;
        }
        let threshold = (self.noise_db + self.margin_db).min(self.absolute_db);
        let loud = db > threshold;
        self.voiced = loud && self.in_speech;

        if !self.in_speech {
            if loud {
                self.above += 1;
                if self.above >= 2 {
                    self.in_speech = true;
                    self.below = 0;
                    self.speech.clear();
                    self.speech.extend(self.pre_roll.drain(..));
                    self.speech.extend_from_slice(frame);
                    self.speech_frames = 2;
                }
            } else {
                self.above = 0;
                self.push_pre_roll(frame);
                self.noise_db = (self.noise_db * 0.995 + db * 0.005).clamp(-90.0, -20.0);
            }
            return None;
        }

        self.speech.extend_from_slice(frame);
        self.speech_frames += 1;
        if loud {
            self.below = 0;
        } else {
            self.below += 1;
        }
        let silence_frames = self.silence_ms.div_ceil(FRAME_MS);
        let max_frames = self.max_utterance_ms.div_ceil(FRAME_MS);
        let done_silence = self.below as u64 >= silence_frames.max(1);
        let done_max = self.speech_frames >= max_frames.max(1);
        if done_silence || done_max {
            self.in_speech = false;
            self.above = 0;
            self.below = 0;
            let samples = std::mem::take(&mut self.speech);
            let speech_ms = self.speech_frames * FRAME_MS;
            self.speech_frames = 0;
            if speech_ms >= self.min_speech_ms {
                return Some(Utterance { samples, speech_ms });
            }
        }
        None
    }

    fn push_pre_roll(&mut self, frame: &[i16]) {
        if self.pre_roll_len == 0 {
            return;
        }
        self.pre_roll.extend(frame.iter().copied());
        while self.pre_roll.len() > self.pre_roll_len {
            self.pre_roll.pop_front();
        }
    }
}

pub fn dbfs(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return -120.0;
    }
    let sum: f64 = samples
        .iter()
        .map(|s| {
            let v = f64::from(*s) / 32768.0;
            v * v
        })
        .sum();
    let rms = (sum / samples.len() as f64).sqrt();
    if rms <= 1e-7 {
        -120.0
    } else {
        (20.0 * rms.log10()) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    fn noise(n: usize, amp: f32, seed: &mut u32) -> Vec<i16> {
        (0..n)
            .map(|_| {
                *seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                let v = ((*seed >> 16) as f32 / 32768.0 - 0.5) * 2.0 * amp;
                (v * 32767.0) as i16
            })
            .collect()
    }

    fn tone(n: usize, hz: f32, amp: f32) -> Vec<i16> {
        (0..n)
            .map(|i| (amp * (2.0 * PI * hz * i as f32 / 16000.0).sin() * 32767.0) as i16)
            .collect()
    }

    fn vad() -> Vad {
        Vad::new(16_000, 200, 300, 2_000, 9.0, 200, -18.0)
    }

    #[test]
    fn detects_tone_after_noise_with_preroll() {
        let mut seed = 7;
        let mut v = vad();
        let mut out = Vec::new();
        out.extend(v.push(&noise(16_000, 0.01, &mut seed)));
        out.extend(v.push(&tone(8_000, 440.0, 0.3)));
        out.extend(v.push(&noise(8_000, 0.01, &mut seed)));
        assert_eq!(out.len(), 1);
        let u = &out[0];
        assert!(u.speech_ms >= 200);
        assert!(u.samples.len() > 16_000 / 10);
        assert!(u.samples.len() <= 16_000 * 2);
    }

    #[test]
    fn does_not_trigger_on_quiet_noise() {
        let mut seed = 3;
        let mut v = vad();
        let mut out = Vec::new();
        for _ in 0..20 {
            out.extend(v.push(&noise(1600, 0.02, &mut seed)));
        }
        assert!(out.is_empty());
        assert!(v.noise_db() > -60.0);
    }

    #[test]
    fn short_blip_is_discarded() {
        let mut v = vad();
        let mut out = Vec::new();
        out.extend(v.push(&tone(640, 440.0, 0.5))); // 40 мс
        out.extend(v.push(&vec![0; 8000]));
        assert!(out.is_empty());
        assert!(!v.in_speech());
    }

    #[test]
    fn long_speech_is_cut_at_max() {
        let mut v = Vad::new(16_000, 100, 300, 500, 9.0, 0, -18.0);
        let mut out = Vec::new();
        for _ in 0..30 {
            out.extend(v.push(&tone(1600, 440.0, 0.3)));
        }
        assert!(!out.is_empty());
        assert!(out[0].samples.len() <= 16_000 / 2 + 100);
        assert!(out[0].speech_ms <= 500);
    }

    #[test]
    fn frame_energy_is_sane() {
        assert!(dbfs(&vec![0; 320]) < -100.0);
        let t = tone(320, 440.0, 0.5);
        let db = dbfs(&t);
        assert!(db > -12.0 && db < -3.0, "{db}");
    }
}
