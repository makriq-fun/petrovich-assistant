//! Служебные звуки генерируются локально: не нужны opus-файлы прошивки и
//! лишние зависимости. Тоны мягкие: колокольная атака, экспоненциальный
//! спад и длинный release, чтобы сигналы не резали слух.

use std::sync::Arc;

use petrovich_core::errors::Result;
use petrovich_core::events::Emotion;
use petrovich_core::ports::{Chime, ChimeSound, VolumeControl};

use crate::frontend::PlaybackHandle;

const RATE: u32 = 24_000;

pub struct LocalChime {
    playback: PlaybackHandle,
    volume: Arc<dyn VolumeControl>,
}

impl LocalChime {
    pub fn new(playback: PlaybackHandle, volume: Arc<dyn VolumeControl>) -> Self {
        Self { playback, volume }
    }

    fn gain(&self) -> f32 {
        let percent = self
            .volume
            .get()
            .map(|v| v.percent)
            .unwrap_or(100)
            .clamp(1, 100);
        volume_gain(percent)
    }
}

impl Chime for LocalChime {
    fn play(&self, sound: ChimeSound) -> Result<()> {
        let gain = self.gain();
        let samples = apply_gain(samples(sound), gain);
        self.playback.play_pcm(samples, RATE);
        Ok(())
    }
}

/// Тихая колонка не должна «съедать» служебный сигнал: до +12 дБ цифровой
/// компенсации при низком Master. Выше 70% усиления нет, поэтому сигнал не
/// становится громче речи на нормальной громкости.
pub fn volume_gain(percent: i32) -> f32 {
    let percent = percent.clamp(1, 100) as f32;
    (100.0 / percent).sqrt().clamp(1.0, 4.0)
}

fn apply_gain(mut samples: Vec<i16>, gain: f32) -> Vec<i16> {
    if (gain - 1.0).abs() < f32::EPSILON {
        return samples;
    }
    for sample in &mut samples {
        let scaled = f32::from(*sample) * gain;
        *sample = scaled.clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
    }
    samples
}

/// Одна нота: частота, длительность, амплитуда и скорость затухания.
/// `hz == 0.0` — пауза.
type Note = (f32, u32, f32, f32);

/// Последовательность нот для звука.
pub fn samples(sound: ChimeSound) -> Vec<i16> {
    let seq: &[Note] = match sound {
        // Активация: короткий восходящий интервал. Короче min_speech_ms,
        // чтобы собственный сигнал не стал «фразой», и почти не съедает
        // начало команды сразу после имени.
        ChimeSound::Listening => &[(783.99, 90, 0.20, 2.4), (1046.5, 120, 0.18, 2.8)],
        // Перестали слушать: тихий нисходящий интервал.
        ChimeSound::ListeningEnd => &[(392.0, 180, 0.13, 2.4), (329.63, 260, 0.12, 2.6)],
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
        ChimeSound::Alarm => &[
            (880.0, 220, 0.18, 1.7),
            (0.0, 120, 0.0, 0.0),
            (880.0, 220, 0.18, 1.7),
            (1174.66, 380, 0.17, 2.2),
            (0.0, 350, 0.0, 0.0),
            (880.0, 220, 0.18, 1.7),
            (0.0, 120, 0.0, 0.0),
            (880.0, 220, 0.18, 1.7),
            (1174.66, 520, 0.16, 2.4),
        ],
        ChimeSound::Emotion(e) => emotion_notes(e),
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

/// Библиотека коротких эмоциональных фраз. Все звуки синтезируются локально,
/// поэтому не требуют сторонних лицензий и файлов на колонке.
fn emotion_notes(emotion: Emotion) -> &'static [Note] {
    match emotion {
        Emotion::Happy => &[
            (523.25, 130, 0.17, 2.2),
            (659.25, 130, 0.17, 2.2),
            (783.99, 130, 0.16, 2.2),
            (1046.50, 320, 0.15, 2.8),
        ],
        Emotion::Sad => &[
            (440.0, 260, 0.15, 2.0),
            (349.23, 300, 0.14, 2.1),
            (293.66, 480, 0.13, 2.4),
        ],
        Emotion::Surprised => &[(987.77, 90, 0.19, 3.0), (1567.98, 240, 0.16, 2.6)],
        Emotion::Angry => &[
            (220.0, 180, 0.18, 2.6),
            (233.08, 180, 0.17, 2.6),
            (196.0, 300, 0.16, 2.2),
        ],
        Emotion::Calm => &[(392.0, 320, 0.13, 1.8), (523.25, 520, 0.12, 1.6)],
        Emotion::Greeting => &[
            (523.25, 140, 0.16, 2.2),
            (659.25, 140, 0.15, 2.2),
            (783.99, 140, 0.14, 2.2),
            (659.25, 220, 0.13, 2.4),
        ],
        Emotion::Neutral => &[(440.0, 180, 0.12, 2.6)],
    }
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
    fn low_master_compensates_chime_but_not_above_full_scale() {
        assert!((volume_gain(100) - 1.0).abs() < 0.001);
        assert!(volume_gain(25) > volume_gain(60));
        assert!(volume_gain(25) <= 4.0);
        assert!(volume_gain(7) > 3.0);
        let scaled = apply_gain(vec![20_000, -20_000], 2.0);
        assert_eq!(scaled, vec![i16::MAX, i16::MIN]);
    }

    #[test]
    fn all_chimes_are_soft_and_nonempty() {
        for s in [
            ChimeSound::Listening,
            ChimeSound::ListeningEnd,
            ChimeSound::Heard,
            ChimeSound::Done,
            ChimeSound::Stopped,
            ChimeSound::Cleared,
            ChimeSound::Timer,
            ChimeSound::Alarm,
            ChimeSound::Error,
            ChimeSound::Emotion(Emotion::Happy),
            ChimeSound::Emotion(Emotion::Sad),
            ChimeSound::Emotion(Emotion::Surprised),
            ChimeSound::Emotion(Emotion::Angry),
            ChimeSound::Emotion(Emotion::Calm),
            ChimeSound::Emotion(Emotion::Greeting),
            ChimeSound::Emotion(Emotion::Neutral),
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
