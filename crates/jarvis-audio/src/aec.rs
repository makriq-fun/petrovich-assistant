//! Подавление эха адаптивным фильтром NLMS. Основной путь на Мини 2 — VQE
//! прошивки с аппаратным референсом; этот фильтр нужен, когда VQE недоступен,
//! и как страховка при воспроизведении.

/// Нормированный LMS с оценкой двойного разговора.
pub struct Nlms {
    taps: Vec<f32>,
    history: Vec<f32>,
    pos: usize,
    mu: f32,
    eps: f32,
    /// Оценка мощности остатка и эха для детектора двойного разговора.
    err_pow: f32,
    echo_pow: f32,
}

impl Nlms {
    pub fn new(taps: usize, mu: f32) -> Self {
        assert!(taps > 0);
        Self {
            taps: vec![0.0; taps],
            history: vec![0.0; taps],
            pos: 0,
            mu: mu.clamp(0.001, 1.0),
            eps: 1e-6,
            err_pow: 1.0,
            echo_pow: 1.0,
        }
    }

    pub fn reset(&mut self) {
        self.taps.fill(0.0);
        self.history.fill(0.0);
        self.pos = 0;
    }

    /// Обрабатывает один отсчёт: `mic` — микрофон, `reference` — сигнал
    /// динамика (уже с компенсацией задержки). Возвращает очищенный сигнал.
    pub fn process(&mut self, mic: f32, reference: f32) -> f32 {
        self.history[self.pos] = reference;
        let mut echo = 0.0f32;
        let mut idx = self.pos;
        for t in &self.taps {
            echo += t * self.history[idx];
            idx = if idx == 0 {
                self.taps.len() - 1
            } else {
                idx - 1
            };
        }
        let err = mic - echo;
        self.err_pow = 0.999 * self.err_pow + 0.001 * err * err;
        self.echo_pow = 0.999 * self.echo_pow + 0.001 * echo * echo;
        // Двойной разговор: остаток заметно громче оценки эха — не адаптируемся,
        // чтобы не «съесть» голос пользователя.
        let double_talk = self.err_pow > 4.0 * self.echo_pow + 1e-6;
        if !double_talk {
            let n = self.taps.len();
            let power: f32 = self.history.iter().map(|x| x * x).sum::<f32>() + self.eps;
            let step = self.mu * err / power;
            let mut idx = self.pos;
            for t in &mut self.taps {
                *t += step * self.history[idx];
                idx = if idx == 0 { n - 1 } else { idx - 1 };
            }
        }
        self.pos = (self.pos + 1) % self.taps.len();
        err
    }

    pub fn process_block(&mut self, mic: &[i16], reference: &[i16]) -> Vec<i16> {
        mic.iter()
            .zip(reference)
            .map(|(&m, &r)| {
                let y = self.process(f32::from(m) / 32768.0, f32::from(r) / 32768.0);
                (y.clamp(-1.0, 1.0) * 32767.0).round() as i16
            })
            .collect()
    }

    pub fn taps(&self) -> &[f32] {
        &self.taps
    }
}

/// Задержка референса в отсчётах: сдвиг истории перед фильтром. Отдельный
/// буфер, потому что тракт динамик→микрофон может быть длиннее фильтра.
pub struct DelayLine {
    buf: Vec<f32>,
    pos: usize,
    delay: usize,
}

impl DelayLine {
    pub fn new(delay: usize) -> Self {
        Self {
            buf: vec![0.0; delay.max(1)],
            pos: 0,
            delay,
        }
    }

    pub fn push(&mut self, x: f32) -> f32 {
        if self.delay == 0 {
            return x;
        }
        let out = self.buf[self.pos];
        self.buf[self.pos] = x;
        self.pos = (self.pos + 1) % self.buf.len();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(x: &[i16]) -> f32 {
        (x.iter().map(|v| f32::from(*v) * f32::from(*v)).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn cancels_synthetic_echo() {
        // Эхо = задержанный и ослабленный референс.
        let n = 16_000;
        let mut seed = 1u32;
        let reference: Vec<i16> = (0..n)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                ((seed >> 16) as i16) / 4
            })
            .collect();
        let delay = 40;
        let mic: Vec<i16> = (0..n)
            .map(|i| {
                let echo = if i >= delay {
                    reference[i - delay] / 3
                } else {
                    0
                };
                echo.saturating_add(300)
            })
            .collect();

        let mut aec = Nlms::new(128, 0.5);
        let mut clean = Vec::with_capacity(n);
        for i in 0..n {
            let r = f32::from(reference[i]) / 32768.0;
            clean.push(aec.process(f32::from(mic[i]) / 32768.0, r));
        }
        let tail = &clean[8000..];
        let before = rms(&mic[8000..]) / 32768.0;
        let after = (tail.iter().map(|x| x * x).sum::<f32>() / tail.len() as f32).sqrt();
        assert!(after < before * 0.35, "before {before}, after {after}");
    }

    #[test]
    fn delay_line_shifts_signal() {
        let mut d = DelayLine::new(2);
        assert_eq!(d.push(1.0), 0.0);
        assert_eq!(d.push(2.0), 0.0);
        assert_eq!(d.push(3.0), 1.0);
        assert_eq!(d.push(4.0), 2.0);
    }

    #[test]
    fn silence_stays_silent() {
        let mut aec = Nlms::new(64, 0.3);
        let out = aec.process_block(&vec![0; 1000], &vec![0; 1000]);
        assert!(out.iter().all(|&v| v == 0));
    }
}
