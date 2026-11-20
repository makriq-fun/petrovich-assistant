//! Светодиодное кольцо Мини 2 через sysfs: 6 RGB-пикселей
//! `mini2:{red,green,blue}:ring1..6`.
//!
//! Анимации считаются по времени, а не по номеру кадра: смена частоты
//! обновления не меняет скорость. Плавность обеспечивает вызывающий код
//! (интерполяция цвета между кадрами).

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use petrovich_core::events::{Emotion, UiNotice, VoiceState};
use petrovich_core::profile::HardwareProfile;

pub const DEFAULT_LED_DIR: &str = "/sys/class/leds/";
const PIXELS: usize = 6;

struct Pixel {
    files: [File; 3],
    regular: [bool; 3],
    last: [u8; 3],
}

pub struct Leds {
    pixels: Vec<Pixel>,
    brightness: f32,
    initialized: bool,
}

impl Leds {
    pub fn open(profile: &HardwareProfile, dir: &Path, brightness: f32) -> Result<Self> {
        let mut pixels = Vec::with_capacity(profile.led_count);
        for names in profile.led_names() {
            let mut files = Vec::with_capacity(3);
            for name in &names {
                let path = dir.join(name).join("brightness");
                files.push(
                    OpenOptions::new()
                        .write(true)
                        .open(&path)
                        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?,
                );
            }
            let files: [File; 3] = files.try_into().expect("три канала");
            let regular = {
                let mut r = [false; 3];
                for (i, name) in names.iter().enumerate() {
                    r[i] = std::fs::metadata(dir.join(name).join("brightness"))
                        .map(|m| m.is_file())
                        .unwrap_or(false);
                }
                r
            };
            pixels.push(Pixel {
                files,
                regular,
                last: [255, 255, 255],
            });
        }
        Ok(Self {
            pixels,
            brightness: brightness.clamp(0.0, 1.0),
            initialized: false,
        })
    }

    pub fn len(&self) -> usize {
        self.pixels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pixels.is_empty()
    }

    pub fn set_brightness(&mut self, brightness: f32) {
        self.brightness = brightness.clamp(0.0, 1.0);
    }

    /// Устанавливает цвета всех пикселей; пишет только изменившиеся каналы.
    pub fn set(&mut self, colors: &[[u8; 3]]) -> Result<()> {
        for (i, px) in self.pixels.iter_mut().enumerate() {
            let c = colors.get(i).copied().unwrap_or([0, 0, 0]);
            let scaled = [
                (f32::from(c[0]) * self.brightness).round() as u8,
                (f32::from(c[1]) * self.brightness).round() as u8,
                (f32::from(c[2]) * self.brightness).round() as u8,
            ];
            for (ch, v) in scaled.iter().enumerate() {
                if self.initialized && *v == px.last[ch] {
                    continue;
                }
                store(&mut px.files[ch], v.to_string().as_bytes(), px.regular[ch])?;
                px.last[ch] = *v;
            }
        }
        self.initialized = true;
        Ok(())
    }

    pub fn clear(&mut self) -> Result<()> {
        self.set(&[[0, 0, 0]; PIXELS])
    }
}

/// Пишет значение в sysfs; обычный файл (тесты) перезаписывает, а не
/// дописывает в конец.
fn store(f: &mut File, value: &[u8], regular: bool) -> std::io::Result<()> {
    if regular {
        f.seek(SeekFrom::Start(0))?;
        f.write_all(value)?;
        f.set_len(value.len() as u64)?;
        f.flush()
    } else {
        f.write_all(value)
    }
}

fn smoothstep(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

fn pulse(t: f32, period: f32) -> f32 {
    0.5 - 0.5 * (2.0 * std::f32::consts::PI * t / period).cos()
}

fn color(r: f32, g: f32, b: f32) -> [u8; 3] {
    [
        (r.clamp(0.0, 1.0) * 255.0).round() as u8,
        (g.clamp(0.0, 1.0) * 255.0).round() as u8,
        (b.clamp(0.0, 1.0) * 255.0).round() as u8,
    ]
}

/// Базовые цвета состояния. `t` — секунды с начала анимации.
pub fn state_colors(state: VoiceState, t: f32) -> Vec<[u8; 3]> {
    let mut out = vec![[0u8; 3]; PIXELS];
    match state {
        VoiceState::Idle => {}
        VoiceState::Listening => {
            // Медленное «дыхание» синью: 2,4 с на цикл.
            let level = 0.22 + 0.78 * smoothstep(pulse(t, 2.4));
            out.fill(color(0.0, 0.18, level));
        }
        VoiceState::Thinking => {
            // Бегущая точка с мягким хвостом; позиция непрерывна.
            let pos = (t / 1.35 * PIXELS as f32).rem_euclid(PIXELS as f32);
            for (i, p) in out.iter_mut().enumerate() {
                let d = (i as f32 - pos).rem_euclid(PIXELS as f32);
                let d = d.min(PIXELS as f32 - d);
                let level = (1.0 - d / 2.2).max(0.0).powf(1.6);
                *p = color(0.0, 0.55 * level + 0.04, 0.75 * level + 0.05);
            }
        }
        VoiceState::Speaking => {
            // Пульс в такт речи; период заметно быстрее «дыхания».
            let level = 0.35 + 0.65 * pulse(t, 1.1);
            out.fill(color(0.0, 0.72 * level, 0.30 * level));
        }
        VoiceState::Error => {
            let level = 0.45 + 0.55 * pulse(t, 0.7);
            out.fill(color(0.85 * level, 0.03 * level, 0.03 * level));
        }
    }
    out
}

/// Кратковременное событие поверх состояния. `t` — секунды с начала.
pub fn notice_colors(notice: UiNotice, t: f32) -> Vec<[u8; 3]> {
    let mut out = vec![[0u8; 3]; PIXELS];
    match notice {
        UiNotice::Listening => {
            let level = (-t / 0.45).exp();
            out.fill(color(0.0, 0.25 * level, 0.9 * level));
        }
        UiNotice::Heard => {
            let level = (-t / 0.20).exp();
            out.fill(color(0.0, 0.55 * level, 0.75 * level));
        }
        UiNotice::Done => {
            let level = (-t / 1.0).exp() * (0.55 + 0.45 * pulse(t, 0.55));
            out.fill(color(0.0, 0.85 * level, 0.35 * level));
        }
        UiNotice::Stopped => {
            let level = (-t / 0.55).exp();
            out.fill(color(0.95 * level, 0.27 * level, 0.0));
        }
        UiNotice::Cleared => {
            // Белая волна слева направо.
            let pos = (t / 0.75 * PIXELS as f32).rem_euclid(PIXELS as f32);
            let fade = (-(t - 0.9).max(0.0) / 0.45).exp();
            for (i, p) in out.iter_mut().enumerate() {
                let d = (i as f32 - pos).rem_euclid(PIXELS as f32);
                let d = d.min(PIXELS as f32 - d);
                let level = (1.0 - d / 1.8).max(0.0) * fade;
                *p = color(0.65 * level, 0.78 * level, 1.0 * level);
            }
        }
        UiNotice::Error => {
            let level = 0.55 + 0.45 * pulse(t, 0.45);
            out.fill(color(0.95 * level, 0.02 * level, 0.02 * level));
        }
        UiNotice::Timer => {
            let level = 0.45 + 0.55 * pulse(t, 0.55);
            out.fill(color(1.0 * level, 0.82 * level, 0.55 * level));
        }
        UiNotice::Alarm => {
            let level = 0.55 + 0.45 * pulse(t, 0.42);
            out.fill(color(1.0 * level, 0.30 * level, 0.02 * level));
        }
        UiNotice::Emotion(e) => return emotion_colors(e, t),
    }
    out
}

/// Цветовая «мимика» кольца для эмоций.
pub fn emotion_colors(emotion: Emotion, t: f32) -> Vec<[u8; 3]> {
    let mut out = vec![[0u8; 3]; PIXELS];
    match emotion {
        Emotion::Happy => {
            let level = 0.45 + 0.55 * pulse(t, 0.8);
            out.fill(color(1.0 * level, 0.62 * level, 0.08 * level));
        }
        Emotion::Sad => {
            let level = (-t / 1.6).exp();
            out.fill(color(0.08 * level, 0.24 * level, 0.85 * level));
        }
        Emotion::Surprised => {
            let level = 0.4 + 0.6 * pulse(t, 0.25);
            out.fill(color(0.15 * level, 0.85 * level, 1.0 * level));
        }
        Emotion::Angry => {
            let level = 0.5 + 0.5 * pulse(t, 0.36);
            out.fill(color(1.0 * level, 0.02 * level, 0.02 * level));
        }
        Emotion::Calm => {
            let level = 0.3 + 0.7 * smoothstep(pulse(t, 2.0));
            out.fill(color(0.02 * level, 0.55 * level, 0.42 * level));
        }
        Emotion::Greeting => {
            let pos = (t / 0.9 * PIXELS as f32).rem_euclid(PIXELS as f32);
            for (i, p) in out.iter_mut().enumerate() {
                let d = (i as f32 - pos).rem_euclid(PIXELS as f32);
                let d = d.min(PIXELS as f32 - d);
                let level = (1.0 - d / 2.0).max(0.0);
                *p = color(0.15 * level, 0.9 * level, 0.35 * level);
            }
        }
        Emotion::Neutral => {
            let level = (-t / 1.2).exp() * 0.7;
            out.fill(color(0.6 * level, 0.65 * level, 0.7 * level));
        }
    }
    out
}

/// Красная индикация мута поверх состояния.
pub fn mute_colors(t: f32) -> Vec<[u8; 3]> {
    let level = 0.55 + 0.45 * pulse(t, 1.6);
    vec![color(0.55 * level, 0.02 * level, 0.02 * level); PIXELS]
}

/// Показ уровня громкости: заполненная дуга из 6 пикселей.
pub fn volume_colors(percent: i32) -> Vec<[u8; 3]> {
    let n = ((percent.clamp(0, 100) as f32 / 100.0) * PIXELS as f32).round() as usize;
    (0..PIXELS)
        .map(|i| {
            if i < n {
                color(0.0, 0.55, 0.85)
            } else {
                [0, 0, 0]
            }
        })
        .collect()
}

/// Погода: тёплые оттенки для положительной температуры, холодные — для
/// отрицательной. Яркость мягко дышит.
pub fn weather_colors(temp_c: f32, t: f32) -> Vec<[u8; 3]> {
    let level = 0.35 + 0.65 * (0.5 + 0.5 * (t * 1.4).sin());
    let warm = (temp_c.clamp(-30.0, 35.0) + 30.0) / 65.0;
    let r = 0.9 * warm + 0.15 * (1.0 - warm);
    let g = 0.55 + 0.15 * warm;
    let b = 0.95 * (1.0 - warm) + 0.2 * warm;
    vec![color(r * level, g * level, b * level); PIXELS]
}

/// Сахар: зелёный в норме, жёлтый при отклонении, красный при опасном
/// значении. Тренд задаёт направление бегущего импульса.
pub fn glucose_colors(mmol: f32, trend: &str, t: f32) -> Vec<[u8; 3]> {
    let (r, g, b) = if mmol < 3.9 {
        (0.95, 0.25, 0.05)
    } else if mmol > 10.0 {
        (1.0, 0.65, 0.05)
    } else {
        (0.1, 0.85, 0.35)
    };
    let forward = !trend.to_lowercase().contains("down") && !trend.contains('↓');
    let pos = (t * 1.6 * PIXELS as f32) % PIXELS as f32;
    let pos = if forward { pos } else { PIXELS as f32 - pos };
    (0..PIXELS)
        .map(|i| {
            let d = (i as f32 - pos)
                .abs()
                .min(PIXELS as f32 - (i as f32 - pos).abs());
            let level = (1.0 - d / 2.5).max(0.0);
            color(r * level, g * level, b * level)
        })
        .collect()
}

/// Воспроизведение: зелёная «эквалайзерная» волна; пауза — ровный тусклый
/// свет. Кольцо остаётся читаемым, но не отвлекает.
pub fn playback_colors(playing: bool, t: f32) -> Vec<[u8; 3]> {
    if !playing {
        return vec![color(0.0, 0.12, 0.08); PIXELS];
    }
    (0..PIXELS)
        .map(|i| {
            let level = 0.18 + 0.82 * (0.5 + 0.5 * (t * 5.0 + i as f32 * 0.9).sin());
            color(0.0, 0.75 * level, 0.35 * level)
        })
        .collect()
}

/// Пути для тестов и диагностики.
pub fn led_paths(profile: &HardwareProfile, dir: &Path) -> Vec<PathBuf> {
    profile
        .led_names()
        .into_iter()
        .flat_map(|names| names.map(|n| dir.join(n).join("brightness")))
        .collect()
}

/// Длительность кратковременных событий на кольце.
pub fn notice_duration(notice: UiNotice) -> Duration {
    match notice {
        UiNotice::Listening | UiNotice::Heard => Duration::from_millis(700),
        UiNotice::Done | UiNotice::Stopped | UiNotice::Cleared => Duration::from_millis(1200),
        UiNotice::Error => Duration::from_millis(1800),
        UiNotice::Timer => Duration::from_millis(2400),
        UiNotice::Alarm => Duration::from_millis(3200),
        UiNotice::Emotion(_) => Duration::from_millis(2500),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use petrovich_core::profile::MINI2;

    fn tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for names in MINI2.led_names() {
            for n in names {
                let p = d.path().join(n);
                std::fs::create_dir_all(&p).unwrap();
                std::fs::write(p.join("brightness"), "0").unwrap();
            }
        }
        d
    }

    #[test]
    fn writes_scaled_colors_and_skips_unchanged() {
        let d = tree();
        let mut l = Leds::open(&MINI2, d.path(), 0.5).unwrap();
        l.set(&[[200, 100, 0]; 6]).unwrap();
        let read = |n: &str| std::fs::read_to_string(d.path().join(n).join("brightness")).unwrap();
        assert_eq!(read("mini2:red:ring1"), "100");
        assert_eq!(read("mini2:green:ring1"), "50");
        assert_eq!(read("mini2:blue:ring6"), "0");
        l.clear().unwrap();
        assert_eq!(read("mini2:red:ring1"), "0");
    }

    #[test]
    fn state_and_volume_animations_have_six_pixels() {
        for s in [
            VoiceState::Idle,
            VoiceState::Listening,
            VoiceState::Thinking,
            VoiceState::Speaking,
            VoiceState::Error,
        ] {
            assert_eq!(state_colors(s, 0.7).len(), 6);
        }
        for n in [
            UiNotice::Listening,
            UiNotice::Heard,
            UiNotice::Done,
            UiNotice::Stopped,
            UiNotice::Cleared,
            UiNotice::Error,
            UiNotice::Timer,
            UiNotice::Alarm,
            UiNotice::Emotion(Emotion::Happy),
        ] {
            assert_eq!(notice_colors(n, 0.3).len(), 6);
        }
        assert_eq!(volume_colors(0), vec![[0, 0, 0]; 6]);
        assert_eq!(volume_colors(100), vec![[0, 140, 217]; 6]);
    }

    #[test]
    fn listening_breathing_is_smooth_and_bounded() {
        let a = state_colors(VoiceState::Listening, 0.0)[0][2];
        let b = state_colors(VoiceState::Listening, 1.2)[0][2];
        assert_ne!(a, b);
        assert!(a > 0 && b > 0);
    }

    #[test]
    fn missing_leds_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        assert!(Leds::open(&MINI2, d.path(), 1.0).is_err());
    }
}
