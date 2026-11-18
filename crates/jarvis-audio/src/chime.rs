//! Служебные звуки генерируются локально: не нужны opus-файлы прошивки и
//! лишние зависимости. Звук короткий, чтобы не задерживать реакцию.

use jarvis_core::errors::Result;
use jarvis_core::ports::{Chime, ChimeSound};

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

/// Последовательность тонов для звука.
pub fn samples(sound: ChimeSound) -> Vec<i16> {
    let seq: &[(f32, u32, f32)] = match sound {
        ChimeSound::Wake => &[(660.0, 70, 0.25), (990.0, 90, 0.28)],
        ChimeSound::Cancel => &[(660.0, 70, 0.25), (440.0, 110, 0.22)],
        ChimeSound::Timer => &[
            (880.0, 90, 0.3),
            (0.0, 40, 0.0),
            (880.0, 90, 0.3),
            (0.0, 40, 0.0),
            (880.0, 140, 0.3),
        ],
        ChimeSound::Error => &[(220.0, 180, 0.25), (185.0, 220, 0.22)],
    };
    let mut out = Vec::new();
    for &(hz, ms, amp) in seq {
        out.extend(tone(hz, ms, amp));
    }
    out
}

/// Синус с плавными фронтами 6 мс, чтобы не было щелчков.
pub fn tone(hz: f32, ms: u32, amp: f32) -> Vec<i16> {
    let n = (RATE as u64 * u64::from(ms) / 1000) as usize;
    let fade = ((RATE as u64 * 6 / 1000) as usize).min(n / 2).max(1);
    (0..n)
        .map(|i| {
            let env = if i < fade {
                i as f32 / fade as f32
            } else if i + fade >= n {
                (n - 1 - i) as f32 / fade as f32
            } else {
                1.0
            };
            let v = if hz <= 0.0 {
                0.0
            } else {
                (2.0 * std::f32::consts::PI * hz * i as f32 / RATE as f32).sin()
            };
            (v * env * amp * 32767.0) as i16
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tones_have_expected_length_and_edges() {
        let t = tone(440.0, 100, 0.5);
        assert_eq!(t.len(), 2400);
        assert_eq!(t[0], 0);
        assert_eq!(*t.last().unwrap(), 0);
        assert!(t.iter().map(|v| v.unsigned_abs()).max().unwrap() > 10_000);
    }

    #[test]
    fn all_chimes_are_short_and_nonempty() {
        for s in [
            ChimeSound::Wake,
            ChimeSound::Cancel,
            ChimeSound::Timer,
            ChimeSound::Error,
        ] {
            let v = samples(s);
            assert!(!v.is_empty());
            assert!(v.len() < RATE as usize);
        }
    }
}
