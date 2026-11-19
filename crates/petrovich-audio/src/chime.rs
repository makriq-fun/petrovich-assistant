//! Служебные звуки генерируются локально: не нужны opus-файлы прошивки и
//! лишние зависимости. Тоны мягкие: колокольная атака, экспоненциальный
//! спад и длинный release, чтобы сигналы не резали слух.

use petrovich_core::errors::Result;
use petrovich_core::ports::{Chime, ChimeSound};

use crate::frontend::PlaybackHandle;

const RATE: u32 = 24_000;

pub struct LocalChime {
    playback: PlaybackHandle,
}

impl LocalChime {
    pub fn new(playback: PlaybackHandle) -> Self {
        Self { playback }
    }
}

impl Chime for LocalChime {
    fn play(&self, sound: ChimeSound) -> Result<()> {
        self.playback.play_pcm(samples(sound), RATE);
        Ok(())
    }
}

/// Одна нота: частота, длительность, амплитуда и скорость затухания.
/// `hz == 0.0` — пауза.
type Note = (f32, u32, f32, f32);

/// Последовательность нот для звука.
pub fn samples(sound: ChimeSound) -> Vec<i16> {
    let seq: &[Note] = match sound {
        // Активация: мягкий восходящий интервал, не слишком громко.
        ChimeSound::Listening => &[(523.25, 240, 0.20, 2.2), (783.99, 360, 0.18, 2.6)],
        // «Услышал»: короткий тихий отклик, чтобы не перебивать речь.
        ChimeSound::Heard => &[(440.0, 130, 0.12, 3.0), (554.37, 170, 0.10, 3.0)],
        // Ответ закончен: спокойный нисходящий интервал.
        ChimeSound::Done => &[(783.99, 190, 0.16, 2.4), (659.25, 300, 0.15, 2.8)],
        // Остановка: низкий мягкий сигнал.
        ChimeSound::Stopped => &[(329.63, 220, 0.18, 2.0), (261.63, 320, 0.16, 2.2)],
        // Очистка: короткое восходящее арпеджио.
        ChimeSound::Cleared => &[
            (523.25, 150, 0.14, 2.5),
            (659.25, 150, 0.13, 2.5),
            (783.99, 260, 0.12, 2.8),
        ],
        // Ошибка: низкие тоны без резкой атаки.
        ChimeSound::Error => &[(220.0, 260, 0.18, 1.8), (174.61, 380, 0.16, 2.0)],
        ChimeSound::Timer => &[
            (659.25, 180, 0.18, 2.0),
            (783.99, 180, 0.18, 2.0),
            (987.77, 180, 0.17, 2.0),
            (1318.51, 340, 0.16, 2.6),
            (0.0, 220, 0.0, 0.0),
            (659.25, 180, 0.16, 2.0),
            (783.99, 180, 0.16, 2.0),
            (987.77, 180, 0.15, 2.0),
            (1318.51, 420, 0.14, 2.8),
        ],
    };
    let mut out = Vec::new();
    for &(hz, ms, amp, decay) in seq {
        if hz <= 0.0 {
            out.extend(std::iter::repeat_n(0, samples_for(ms)));
        } else {
            out.extend(tone(hz, ms, amp, decay));
        }
    }
    out
}

fn samples_for(ms: u32) -> usize {
    (u64::from(RATE) * u64::from(ms) / 1000) as usize
}

/// Синус с мягкой атакой и длинным release. Лёгкие гармоники делают звук
/// колокольным, а не «пищалкой».
pub fn tone(hz: f32, ms: u32, amp: f32, decay: f32) -> Vec<i16> {
    let n = samples_for(ms);
    if n == 0 {
        return Vec::new();
    }
    let attack = ((RATE as f32 * 0.018) as usize).min(n / 3).max(1);
    let release = ((RATE as f32 * 0.090) as usize).min(n / 2).max(1);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let up = smoothstep(i as f32 / attack as f32);
        let down = if i + release >= n {
            smoothstep((n - 1 - i) as f32 / release as f32)
        } else {
            1.0
        };
        let env = up * down * (-decay * i as f32 / n as f32).exp();
        let phase = 2.0 * std::f32::consts::PI * hz * i as f32 / RATE as f32;
        let wave = phase.sin() + 0.28 * (2.0 * phase).sin() + 0.08 * (3.0 * phase).sin();
        let v = wave * 0.72 * env * amp;
        out.push((v * 32767.0).clamp(-32768.0, 32767.0) as i16);
    }
    // Гарантируем нулевые края: щелчков не будет даже при округлении.
    if let Some(first) = out.first_mut() {
        *first = 0;
    }
    if let Some(last) = out.last_mut() {
        *last = 0;
    }
    out
}

fn smoothstep(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tones_have_expected_length_and_edges() {
        let t = tone(440.0, 100, 0.5, 2.0);
        assert_eq!(t.len(), 2400);
        assert_eq!(t[0], 0);
        assert_eq!(*t.last().unwrap(), 0);
        assert!(t.iter().map(|v| v.unsigned_abs()).max().unwrap() > 8_000);
    }

    #[test]
    fn all_chimes_are_soft_and_nonempty() {
        for s in [
            ChimeSound::Listening,
            ChimeSound::Heard,
            ChimeSound::Done,
            ChimeSound::Stopped,
            ChimeSound::Cleared,
            ChimeSound::Timer,
            ChimeSound::Error,
        ] {
            let v = samples(s);
            assert!(!v.is_empty());
            assert!(v.len() < RATE as usize * 4);
            assert_eq!(v[0], 0);
            assert_eq!(*v.last().unwrap(), 0);
            let peak = v.iter().map(|v| v.unsigned_abs()).max().unwrap();
            assert!(peak < 12_000, "слишком громкий сигнал: {peak}");
        }
    }
}
