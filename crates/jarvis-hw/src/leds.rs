//! Светодиодное кольцо Мини 2 через sysfs: 6 RGB-пикселей
//! `mini2:{red,green,blue}:ring1..6`.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use jarvis_core::profile::HardwareProfile;

pub const DEFAULT_LED_DIR: &str = "/sys/class/leds/";

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
        self.set(&[[0, 0, 0]; 6])
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

/// Цвета состояний.
pub fn state_colors(state: jarvis_core::events::VoiceState, tick: u64) -> Vec<[u8; 3]> {
    use jarvis_core::events::VoiceState as S;
    let mut out = vec![[0u8; 3]; 6];
    let phase = (tick % 40) as f32 / 40.0;
    match state {
        S::Idle => {}
        S::Listening => {
            let b = (0.35 + 0.65 * (phase * std::f32::consts::PI).sin()) * 255.0;
            out.fill([0, 40, b as u8]);
        }
        S::Thinking => {
            for (i, p) in out.iter_mut().enumerate() {
                let d = (i as f32 / 6.0 - phase).rem_euclid(1.0);
                let b = (1.0 - d * 3.0).max(0.0) * 255.0;
                *p = [0, b as u8, b as u8];
            }
        }
        S::Speaking => {
            let b = (0.5 + 0.5 * (phase * std::f32::consts::PI * 2.0).sin()) * 200.0;
            out.fill([0, b as u8, 40]);
        }
        S::Error => {
            out.fill([200, 0, 0]);
        }
    }
    out
}

/// Красная индикация мута поверх состояния.
pub fn mute_colors() -> Vec<[u8; 3]> {
    vec![[120, 0, 0]; 6]
}

/// Показ уровня громкости: заполненная дуга из 6 пикселей.
pub fn volume_colors(percent: i32) -> Vec<[u8; 3]> {
    let n = ((percent.clamp(0, 100) as f32 / 100.0) * 6.0).round() as usize;
    (0..6)
        .map(|i| if i < n { [0, 120, 200] } else { [0, 0, 0] })
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

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_core::profile::MINI2;

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
            jarvis_core::events::VoiceState::Idle,
            jarvis_core::events::VoiceState::Listening,
            jarvis_core::events::VoiceState::Thinking,
            jarvis_core::events::VoiceState::Speaking,
            jarvis_core::events::VoiceState::Error,
        ] {
            assert_eq!(state_colors(s, 3).len(), 6);
        }
        assert_eq!(volume_colors(0), vec![[0, 0, 0]; 6]);
        assert_eq!(volume_colors(100), vec![[0, 120, 200]; 6]);
    }

    #[test]
    fn missing_leds_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        assert!(Leds::open(&MINI2, d.path(), 1.0).is_err());
    }
}
