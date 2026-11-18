//! Ресемплинг для тракта колонки. Захват 48 кГц -> 16 кГц делается FIR-
//! дециматором (качество важно для распознавания), TTS -> 48 кГц — линейным
//! интерполятором с предварительным ФНЧ при понижении.

/// КИХ-фильтр нижних частот с децимацией на целый коэффициент.
pub struct Decimator {
    factor: usize,
    taps: Vec<f32>,
    history: Vec<f32>,
    pos: usize,
}

impl Decimator {
    pub fn new(input_rate: u32, output_rate: u32) -> Self {
        assert!(input_rate >= output_rate && input_rate % output_rate == 0);
        let factor = (input_rate / output_rate) as usize;
        let cutoff = 0.45 / factor as f32; // доля частоты дискретизации входа
        let len = 63usize;
        let mid = (len - 1) as f32 / 2.0;
        let mut taps = Vec::with_capacity(len);
        let mut sum = 0.0f32;
        for i in 0..len {
            let x = i as f32 - mid;
            let sinc = if x.abs() < 1e-6 {
                2.0 * cutoff
            } else {
                (2.0 * std::f32::consts::PI * cutoff * x).sin() / (std::f32::consts::PI * x)
            };
            let window =
                0.54 - 0.46 * (2.0 * std::f32::consts::PI * i as f32 / (len - 1) as f32).cos();
            let v = sinc * window;
            taps.push(v);
            sum += v;
        }
        for t in &mut taps {
            *t /= sum;
        }
        Self {
            factor,
            taps,
            history: vec![0.0; len],
            pos: 0,
        }
    }

    pub fn factor(&self) -> usize {
        self.factor
    }

    /// Фильтрует и прореживает; длина результата — ceil(len/factor).
    pub fn process(&mut self, input: &[i16]) -> Vec<i16> {
        let mut out = Vec::with_capacity(input.len() / self.factor + 1);
        for &s in input {
            self.history.remove(0);
            self.history.push(f32::from(s) / 32768.0);
            if self.pos % self.factor == 0 {
                let y: f32 = self
                    .history
                    .iter()
                    .zip(&self.taps)
                    .map(|(x, t)| x * t)
                    .sum();
                out.push((y.clamp(-1.0, 1.0) * 32767.0).round() as i16);
            }
            self.pos = (self.pos + 1) % self.factor;
        }
        out
    }
}

/// Линейный интерполятор с состоянием. Для понижения частоты сначала
/// применяется простой ФНЧ (скользящее среднее), чтобы не было алиасинга.
pub struct LinearResampler {
    step: f64,
    frac: f64,
    prev: f32,
    started: bool,
    down_lp: Option<MovingAverage>,
}

impl LinearResampler {
    pub fn new(input_rate: u32, output_rate: u32) -> Self {
        let step = f64::from(input_rate) / f64::from(output_rate);
        let down_lp = (input_rate > output_rate)
            .then(|| MovingAverage::new((input_rate / output_rate).max(2) as usize));
        Self {
            step,
            frac: 0.0,
            prev: 0.0,
            started: false,
            down_lp,
        }
    }

    pub fn process(&mut self, input: &[i16]) -> Vec<i16> {
        let filtered: Vec<f32> = match &mut self.down_lp {
            Some(lp) => input.iter().map(|&s| lp.push(f32::from(s))).collect(),
            None => input.iter().map(|&s| f32::from(s)).collect(),
        };
        let mut out = Vec::with_capacity((filtered.len() as f64 / self.step) as usize + 2);
        for x in filtered {
            if !self.started {
                self.prev = x;
                self.started = true;
                out.push(x.round() as i16);
                continue;
            }
            while self.frac <= 1.0 {
                let y = self.prev + (x - self.prev) * self.frac as f32;
                out.push(y.clamp(-32768.0, 32767.0).round() as i16);
                self.frac += self.step;
            }
            self.frac -= 1.0;
            self.prev = x;
        }
        out
    }
}

struct MovingAverage {
    buf: Vec<f32>,
    pos: usize,
    sum: f32,
}

impl MovingAverage {
    fn new(len: usize) -> Self {
        Self {
            buf: vec![0.0; len],
            pos: 0,
            sum: 0.0,
        }
    }

    fn push(&mut self, v: f32) -> f32 {
        self.sum -= self.buf[self.pos];
        self.buf[self.pos] = v;
        self.sum += v;
        self.pos = (self.pos + 1) % self.buf.len();
        self.sum / self.buf.len() as f32
    }
}

/// Понижение/повышение произвольной частоты одним вызовом (для тестов и
/// коротких буферов). Потоковый тракт использует структуры выше.
pub fn resample(input: &[i16], from: u32, to: u32) -> Vec<i16> {
    if from == to {
        return input.to_vec();
    }
    if from > to && from % to == 0 {
        return Decimator::new(from, to).process(input);
    }
    LinearResampler::new(from, to).process(input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    fn sine(rate: u32, hz: f32, ms: u32, amp: f32) -> Vec<i16> {
        let n = (rate as f32 * ms as f32 / 1000.0) as usize;
        (0..n)
            .map(|i| (amp * (2.0 * PI * hz * i as f32 / rate as f32).sin() * 32767.0) as i16)
            .collect()
    }

    fn rms(x: &[i16]) -> f32 {
        if x.is_empty() {
            return 0.0;
        }
        (x.iter().map(|v| f32::from(*v) * f32::from(*v)).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn decimates_48k_to_16k_preserving_signal() {
        let input = sine(48_000, 1_000.0, 1000, 0.5);
        let out = Decimator::new(48_000, 16_000).process(&input);
        assert!((out.len() as i64 - 16_000).abs() < 50, "len {}", out.len());
        // Отбрасываем переходный процесс фильтра.
        let steady = &out[500..];
        let r = rms(steady);
        assert!(r > 0.25 * 32767.0 && r < 0.45 * 32767.0, "rms {r}");
    }

    #[test]
    fn rejects_aliasing_above_nyquist() {
        // 10 кГц при переходе на 16 кГц должна быть подавлена фильтром.
        let input = sine(48_000, 10_000.0, 500, 0.5);
        let out = Decimator::new(48_000, 16_000).process(&input);
        assert!(rms(&out[500..]) < 0.05 * 32767.0);
    }

    #[test]
    fn upsamples_24k_to_48k() {
        let input = sine(24_000, 440.0, 200, 0.4);
        let out = LinearResampler::new(24_000, 48_000).process(&input);
        assert!((out.len() as i64 - input.len() as i64 * 2).abs() < 4);
        assert!(rms(&out[200..]) > 0.25 * 32767.0);
    }

    #[test]
    fn arbitrary_rates_do_not_panic() {
        let input = sine(22_050, 300.0, 100, 0.3);
        let out = resample(&input, 22_050, 48_000);
        assert!(!out.is_empty());
    }
}
