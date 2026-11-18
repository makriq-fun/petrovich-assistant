//! Мел-фронтенд openWakeWord: 16 кГц, окно 400 из 512, шаг 160, 32 мел-
//! канала, log10 с диапазоном 80 дБ и масштабом x/10+2.

pub const SAMPLE_RATE: u32 = 16_000;
pub const MEL_BINS: usize = 32;
pub const STEP_SAMP: usize = 1280;
pub const STEP_FRAMES: usize = 8;
pub const FFT_SIZE: usize = 512;
pub const HOP: usize = 160;
pub const WIN_LEN: usize = 400;
pub const CTX_SAMP: usize = 3 * HOP;

pub struct MelFront {
    filters: Vec<f32>, // 257 x 32
    window: [f64; FFT_SIZE],
    tw_re: [f64; FFT_SIZE / 2],
    tw_im: [f64; FFT_SIZE / 2],
    re: [f64; FFT_SIZE],
    im: [f64; FFT_SIZE],
    power: [f64; FFT_SIZE / 2 + 1],
}

impl MelFront {
    pub fn new(filters: &[f32]) -> anyhow::Result<Self> {
        let bins = FFT_SIZE / 2 + 1;
        anyhow::ensure!(
            filters.len() == bins * MEL_BINS,
            "мел-фильтры: {} значений, нужно {}",
            filters.len(),
            bins * MEL_BINS
        );
        let mut m = Self {
            filters: filters.to_vec(),
            window: [0.0; FFT_SIZE],
            tw_re: [0.0; FFT_SIZE / 2],
            tw_im: [0.0; FFT_SIZE / 2],
            re: [0.0; FFT_SIZE],
            im: [0.0; FFT_SIZE],
            power: [0.0; FFT_SIZE / 2 + 1],
        };
        let off = (FFT_SIZE - WIN_LEN) / 2;
        for i in 0..WIN_LEN {
            m.window[off + i] =
                0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / WIN_LEN as f64).cos();
        }
        for i in 0..FFT_SIZE / 2 {
            let a = -2.0 * std::f64::consts::PI * i as f64 / FFT_SIZE as f64;
            m.tw_re[i] = a.cos();
            m.tw_im[i] = a.sin();
        }
        Ok(m)
    }

    fn fft(&mut self) {
        let n = FFT_SIZE;
        let mut j = 0usize;
        for i in 1..n {
            let mut bit = n >> 1;
            while j & bit != 0 {
                j ^= bit;
                bit >>= 1;
            }
            j ^= bit;
            if i < j {
                self.re.swap(i, j);
                self.im.swap(i, j);
            }
        }
        let mut size = 2;
        while size <= n {
            let half = size >> 1;
            let step = n / size;
            let mut start = 0;
            while start < n {
                for k in 0..half {
                    let t_re = self.tw_re[k * step] * self.re[start + k + half]
                        - self.tw_im[k * step] * self.im[start + k + half];
                    let t_im = self.tw_re[k * step] * self.im[start + k + half]
                        + self.tw_im[k * step] * self.re[start + k + half];
                    self.re[start + k + half] = self.re[start + k] - t_re;
                    self.im[start + k + half] = self.im[start + k] - t_im;
                    self.re[start + k] += t_re;
                    self.im[start + k] += t_im;
                }
                start += size;
            }
            size <<= 1;
        }
    }

    /// Считает мел-кадры из `samples` (int16-масштаб во f32), пишет в `dst`.
    pub fn frames(&mut self, samples: &[f32], dst: &mut [f32]) -> usize {
        let mut count = 0usize;
        let mut start = 0usize;
        while start + FFT_SIZE <= samples.len() {
            for i in 0..FFT_SIZE {
                self.re[i] = f64::from(samples[start + i]) * self.window[i];
                self.im[i] = 0.0;
            }
            self.fft();
            for k in 0..=FFT_SIZE / 2 {
                self.power[k] = self.re[k] * self.re[k] + self.im[k] * self.im[k];
            }
            let out = &mut dst[count * MEL_BINS..(count + 1) * MEL_BINS];
            for (b, slot) in out.iter_mut().enumerate() {
                let mut s = 0.0f64;
                for k in 0..=FFT_SIZE / 2 {
                    s += self.power[k] * f64::from(self.filters[k * MEL_BINS + b]);
                }
                *slot = (10.0 * s.max(1e-10).log10()) as f32;
            }
            count += 1;
            start += HOP;
        }
        let mut top = f32::NEG_INFINITY;
        for &v in &dst[..count * MEL_BINS] {
            top = top.max(v);
        }
        for v in &mut dst[..count * MEL_BINS] {
            *v = (*v).max(top - 80.0) / 10.0 + 2.0;
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zero_filters() -> Vec<f32> {
        vec![0.0; (FFT_SIZE / 2 + 1) * MEL_BINS]
    }

    #[test]
    fn produces_eight_frames_per_step() {
        let mut m = MelFront::new(&zero_filters()).unwrap();
        let mut dst = vec![0.0; (STEP_FRAMES + 1) * MEL_BINS];
        let n = m.frames(&vec![0.0; CTX_SAMP + STEP_SAMP], &mut dst);
        assert_eq!(n, STEP_FRAMES);
        // Нулевой спектр: log10(1e-10) = -100 -> после clamp/scale 2-8.
        assert!((dst[0] - (-8.0)).abs() < 1e-5, "{}", dst[0]);
    }

    #[test]
    fn sine_energy_is_higher_than_silence() {
        let filters: Vec<f32> = (0..(FFT_SIZE / 2 + 1) * MEL_BINS)
            .map(|i| ((i / MEL_BINS) as f32 / 257.0).sin())
            .collect();
        let mut m = MelFront::new(&filters).unwrap();
        let mut dst = vec![0.0; (STEP_FRAMES + 1) * MEL_BINS];
        let samples: Vec<f32> = (0..CTX_SAMP + STEP_SAMP)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 16000.0).sin() * 8000.0)
            .collect();
        let n = m.frames(&samples, &mut dst);
        assert!(dst[..n * MEL_BINS].iter().any(|v| *v > 3.0));
    }
}
