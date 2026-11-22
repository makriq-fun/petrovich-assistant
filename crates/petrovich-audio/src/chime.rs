//! Служебные звуки генерируются локально: не нужны opus-файлы прошивки и
//! лишние зависимости. Тоны мягкие: колокольная атака, экспоненциальный
//! спад и длинный release, чтобы сигналы не резали слух.

use std::sync::Arc;

use crate::frontend::PlaybackHandle;
use petrovich_core::errors::Result;
use petrovich_core::events::Emotion;
use petrovich_core::ports::{Chime, ChimeSound, VolumeControl};

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
            .cached_percent()
            .or_else(|| self.volume.get().ok().map(|v| v.percent))
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

/// Потолок цифрового сигнала. Оставляем запас, чтобы округление и
/// наложение с TTS не срезали вершины в щелчок.
const PEAK_CEILING: f32 = 0.92;

/// Тихая колонка не должна «съедать» служебный сигнал: до +12 дБ цифровой
/// компенсации при низком Master. Выше 70% усиления нет, поэтому сигнал не
/// становится громче речи на нормальной громкости.
pub fn volume_gain(percent: i32) -> f32 {
    let percent = percent.clamp(1, 100) as f32;
    (100.0 / percent).sqrt().clamp(1.0, 4.0)
}

/// Усиливает сигнал, но не даёт вершине выйти за [`PEAK_CEILING`].
/// Масштабируется весь сигнал целиком: форма и тембр сохраняются, вместо
/// жёсткого клиппинга, который добавляет в звук неприятные гармоники.
fn apply_gain(mut samples: Vec<i16>, gain: f32) -> Vec<i16> {
    let peak = samples
        .iter()
        .map(|s| i32::from(*s).unsigned_abs())
        .max()
        .unwrap_or(0) as f32;
    if peak == 0.0 {
        return samples;
    }
    let ceiling = PEAK_CEILING * f32::from(i16::MAX);
    let scale = gain.min(ceiling / peak);
    if (scale - 1.0).abs() < f32::EPSILON {
        return samples;
    }
    for sample in &mut samples {
        let scaled = (f32::from(*sample) * scale).round();
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
        // Активация: короткий восходящий интервал в нижней середине.
        // 250 мс суммарно — меньше min_speech_ms, чтобы сигнал не стал
        // «фразой» и не съел начало команды сразу после имени.
        ChimeSound::Listening => &[(392.0, 110, 0.58, 2.4), (523.25, 140, 0.55, 2.6)],
        // Перестали слушать: три нисходящие ноты, заметно медленнее
        // «готово» — по контуру понятно, что диалог закрылся.
        ChimeSound::ListeningEnd => &[
            (329.63, 180, 0.45, 1.8),
            (293.66, 180, 0.43, 1.9),
            (246.94, 300, 0.42, 2.0),
        ],
        // «Услышал»: один короткий низкий «ток», а не два писка. Он
        // подтверждает захват фразы, но не перебивает размышление.
        ChimeSound::Heard => &[(329.63, 110, 0.40, 3.2)],
        // Ответ закончен: две ноты вниз, короче и выше закрытия слушания.
        // 300 мс: сигнал успевает прозвучать до начала follow-up, а не
        // превращается в «речь», которую VAD потом принимает за команду.
        ChimeSound::Done => &[(523.25, 120, 0.52, 2.2), (392.0, 180, 0.48, 2.4)],
        // Остановка: низкий двойной удар.
        ChimeSound::Stopped => &[(246.94, 150, 0.52, 2.5), (196.0, 260, 0.48, 2.7)],
        // Очистка: восходящее арпеджио E-G-C, без верхней октавы.
        ChimeSound::Cleared => &[
            (329.63, 110, 0.48, 2.4),
            (392.0, 110, 0.48, 2.4),
            (523.25, 280, 0.46, 2.6),
        ],
        // Ошибка: низкие ноты с малым секундным трением. Не «пищит»,
        // но на слух сразу отличается от рабочего сигнала.
        ChimeSound::Error => &[(196.0, 240, 0.60, 1.8), (185.0, 380, 0.56, 2.0)],
        ChimeSound::Timer => &[
            (440.0, 170, 0.50, 1.9),
            (554.37, 170, 0.50, 1.9),
            (659.25, 220, 0.48, 2.0),
            (0.0, 150, 0.0, 0.0),
            (440.0, 170, 0.48, 1.9),
            (554.37, 170, 0.48, 1.9),
            (659.25, 340, 0.46, 2.2),
        ],
        ChimeSound::Alarm => &[
            (392.0, 200, 0.60, 1.7),
            (0.0, 90, 0.0, 0.0),
            (392.0, 200, 0.60, 1.7),
            (523.25, 320, 0.55, 1.9),
            (0.0, 250, 0.0, 0.0),
            (392.0, 200, 0.60, 1.7),
            (0.0, 90, 0.0, 0.0),
            (392.0, 200, 0.60, 1.7),
            (523.25, 420, 0.52, 2.0),
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
            (392.0, 120, 0.50, 2.2),
            (523.25, 120, 0.50, 2.2),
            (659.25, 120, 0.48, 2.2),
            (783.99, 280, 0.45, 2.5),
        ],
        Emotion::Sad => &[
            (349.23, 220, 0.40, 2.0),
            (293.66, 260, 0.38, 2.1),
            (246.94, 400, 0.36, 2.3),
        ],
        Emotion::Surprised => &[(523.25, 90, 0.55, 3.0), (659.25, 220, 0.50, 2.6)],
        Emotion::Angry => &[
            (196.0, 160, 0.55, 2.6),
            (185.0, 160, 0.55, 2.6),
            (164.81, 300, 0.50, 2.2),
        ],
        Emotion::Calm => &[(261.63, 280, 0.38, 1.8), (329.63, 480, 0.36, 1.6)],
        Emotion::Greeting => &[
            (392.0, 120, 0.48, 2.2),
            (523.25, 120, 0.48, 2.2),
            (659.25, 120, 0.46, 2.2),
            (523.25, 220, 0.44, 2.4),
        ],
        Emotion::Neutral => &[(329.63, 150, 0.38, 2.6)],
    }
}

fn samples_for(ms: u32) -> usize {
    (u64::from(RATE) * u64::from(ms) / 1000) as usize
}

/// Тёплый тон с мягкой атакой и длинным release. Основная частота
/// доминирует, вторая гармоника добавляет тело, третья почти убрана:
/// именно верхние гармоники делали прежние сигналы писклявыми. Субоктава
/// даёт низкую «деревянную» опору, чтобы звук был слышен и на слабом
/// динамике колонки.
pub fn tone(hz: f32, ms: u32, amp: f32, decay: f32) -> Vec<i16> {
    let n = samples_for(ms);
    if n == 0 {
        return Vec::new();
    }
    let attack = ((RATE as f32 * 0.012) as usize).min(n / 3).max(1);
    let release = ((RATE as f32 * 0.100) as usize).min(n / 2).max(1);
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
        let wave = phase.sin()
            + 0.12 * (0.5 * phase).sin()
            + 0.22 * (2.0 * phase).sin()
            + 0.05 * (3.0 * phase).sin();
        let v = wave * 0.80 * env * amp;
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
    fn low_master_compensates_chime_but_not_above_ceiling() {
        assert!((volume_gain(100) - 1.0).abs() < 0.001);
        assert!(volume_gain(25) > volume_gain(60));
        assert!(volume_gain(25) <= 4.0);
        assert!(volume_gain(7) > 3.0);
        let scaled = apply_gain(vec![20_000, -20_000], 2.0);
        let peak = scaled.iter().map(|v| v.unsigned_abs()).max().unwrap();
        let ceiling = (PEAK_CEILING * f32::from(i16::MAX)).ceil() as u16;
        assert!(peak <= ceiling, "пик {peak} выше потолка {ceiling}");
        assert_eq!(scaled[0], -scaled[1], "форма сигнала должна сохраниться");
    }

    #[test]
    fn activation_and_heard_fit_vad_min_speech() {
        // Сигнал активации не должен приниматься за начало команды.
        assert!(samples(ChimeSound::Listening).len() < RATE as usize * 300 / 1000);
        assert!(samples(ChimeSound::Heard).len() < RATE as usize * 200 / 1000);
    }

    #[test]
    fn all_chimes_are_audible_bounded_and_distinct() {
        let sounds = [
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
        ];
        for (i, s) in sounds.iter().enumerate() {
            let v = samples(*s);
            assert!(!v.is_empty());
            assert!(v.len() < RATE as usize * 4);
            assert_eq!(v[0], 0);
            assert_eq!(*v.last().unwrap(), 0);
            let peak = v.iter().map(|v| v.unsigned_abs()).max().unwrap();
            assert!(
                (8_000..=24_000).contains(&peak),
                "сигнал {s:?} должен быть слышимым, но не клипповать: {peak}"
            );
            for other in &sounds[i + 1..] {
                assert_ne!(v, samples(*other), "{s:?} и {other:?} звучат одинаково");
            }
        }
    }
}
