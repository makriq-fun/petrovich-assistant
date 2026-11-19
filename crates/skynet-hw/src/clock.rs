//! Часы Мини 2: по одному sysfs-светодиоду на сегмент,
//! `mini2:white:clock{A..G}{1..4}`, двоеточие PLUS1/PLUS2.
//!
//! Яркость двоеточия задаётся отдельно, поэтому его можно плавно гасить, а
//! не переключать рывком раз в секунду.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use skynet_core::profile::HardwareProfile;

pub const DEFAULT_LED_DIR: &str = "/sys/class/leds/";

const DIGITS: [u8; 10] = [0x3f, 0x06, 0x5b, 0x4f, 0x66, 0x6d, 0x7d, 0x07, 0x7f, 0x6f];
const SEGS: usize = 7;

pub struct Clock {
    segments: Vec<File>,
    colon: Vec<File>,
    last: Vec<u8>,
    level: u8,
    enabled: bool,
    initialized: bool,
}

impl Clock {
    pub fn open(profile: &HardwareProfile, dir: &Path, brightness: f32) -> Self {
        let prefix = profile.clock_dir.rsplit('/').next().unwrap_or("clock");
        let mut segments = Vec::new();
        let mut paths: Vec<PathBuf> = Vec::new();
        for digit in 0..4 {
            for seg in 0..SEGS {
                paths.push(
                    dir.join(format!("{prefix}{}", profile.segment_name(digit, seg)))
                        .join("brightness"),
                );
            }
        }
        for p in &paths {
            match OpenOptions::new().write(true).open(p) {
                Ok(f) => segments.push(f),
                Err(_) => {
                    tracing::info!("часы: нет {}", p.display());
                    return Self::disabled();
                }
            }
        }
        let mut colon = Vec::new();
        for name in profile.clock_colon {
            let p = dir.join(format!("{prefix}{name}")).join("brightness");
            match OpenOptions::new().write(true).open(&p) {
                Ok(f) => colon.push(f),
                Err(_) => {
                    tracing::info!("часы: нет {}", p.display());
                    return Self::disabled();
                }
            }
        }
        // Вспомогательные сегменты гасим один раз.
        for name in profile.clock_aux {
            let p = dir.join(format!("{prefix}{name}")).join("brightness");
            if let Ok(mut f) = OpenOptions::new().write(true).open(p) {
                let _ = f.write_all(b"0");
            }
        }
        let level = (brightness.clamp(0.0, 1.0) * 255.0).round() as u8;
        Self {
            last: vec![0; segments.len() + colon.len()],
            segments,
            colon,
            level: level.max(2),
            enabled: true,
            initialized: false,
        }
    }

    fn disabled() -> Self {
        Self {
            segments: Vec::new(),
            colon: Vec::new(),
            last: Vec::new(),
            level: 0,
            enabled: false,
            initialized: false,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn level(&self) -> u8 {
        self.level
    }

    pub fn set_brightness(&mut self, brightness: f32) {
        self.level = (brightness.clamp(0.0, 1.0) * 255.0).round() as u8;
    }

    /// Показывает произвольные маски цифр. `colon_level` — яркость
    /// двоеточия, `level` — яркость сегментов.
    pub fn set_levels(&mut self, bytes: &[u8; 4], colon_level: u8, level: u8) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let mut values = Vec::with_capacity(self.segments.len() + self.colon.len());
        for mask in bytes.iter() {
            let mask = *mask & 0x7f;
            for seg in 0..SEGS {
                values.push(if mask >> seg & 1 != 0 { level } else { 0 });
            }
        }
        for _ in &self.colon {
            values.push(colon_level);
        }
        let first = !self.initialized;
        for (i, f) in self.segments.iter_mut().enumerate() {
            if first || values[i] != self.last[i] {
                store(f, values[i].to_string().as_bytes())?;
                self.last[i] = values[i];
            }
        }
        let off = self.segments.len();
        for (j, f) in self.colon.iter_mut().enumerate() {
            let v = values[off + j];
            if first || v != self.last[off + j] {
                store(f, v.to_string().as_bytes())?;
                self.last[off + j] = v;
            }
        }
        self.initialized = true;
        Ok(())
    }

    /// Показывает маски с текущей яркостью.
    pub fn set_masks(&mut self, masks: &[u8; 4], colon_on: bool) -> Result<()> {
        let colon = if colon_on { self.level } else { 0 };
        self.set_levels(masks, colon, self.level)
    }

    fn set_digits(&mut self, digits: &[u8; 4], colon_level: u8, level: u8) -> Result<()> {
        let masks = [
            DIGITS[(digits[0] as usize).min(9)],
            DIGITS[(digits[1] as usize).min(9)],
            DIGITS[(digits[2] as usize).min(9)],
            DIGITS[(digits[3] as usize).min(9)],
        ];
        self.set_levels(&masks, colon_level, level)
    }

    /// Показывает HH:MM.
    pub fn show_time(&mut self, hour: u32, minute: u32, colon: bool) -> Result<()> {
        let level = if colon { self.level } else { 0 };
        self.show_time_level(hour, minute, level)
    }

    /// Показывает HH:MM с заданной яркостью двоеточия: нужно для плавного
    /// мигания.
    pub fn show_time_level(&mut self, hour: u32, minute: u32, colon_level: u8) -> Result<()> {
        let bytes = [
            ((hour / 10) % 10) as u8,
            (hour % 10) as u8,
            ((minute / 10) % 10) as u8,
            (minute % 10) as u8,
        ];
        self.set_digits(&bytes, colon_level, self.level)
    }

    /// Показывает MM:SS для короткого таймера и MM:00 для длинного:
    /// длинный таймер обновляется раз в минуту, а не каждую секунду.
    pub fn show_timer(&mut self, seconds: u64) -> Result<()> {
        if seconds >= 60 {
            let mins = seconds.div_ceil(60).min(99);
            let bytes = [((mins / 10) % 10) as u8, (mins % 10) as u8, 0, 0];
            return self.set_digits(&bytes, self.level, self.level);
        }
        let s = seconds.min(59);
        let bytes = [0, 0, ((s / 10) % 10) as u8, (s % 10) as u8];
        self.set_digits(&bytes, self.level, self.level)
    }

    pub fn all_off(&mut self) -> Result<()> {
        self.set_levels(&[0; 4], 0, 0)
    }
}

fn store(f: &mut File, value: &[u8]) -> std::io::Result<()> {
    let regular = f.metadata().map(|m| m.is_file()).unwrap_or(false);
    if regular {
        f.seek(SeekFrom::Start(0))?;
    }
    f.write_all(value)?;
    if regular {
        f.set_len(value.len() as u64)?;
    }
    f.flush()
}

/// Глифы цифр для тестов/диагностики.
pub fn digit_mask(d: u8) -> u8 {
    DIGITS[(d as usize).min(9)]
}

/// Один бегущий сегмент по всем четырём цифрам: индикатор «думаю».
/// `t` — секунды, шаг — 120 мс.
pub fn spinner_masks(t: f32) -> [u8; 4] {
    let pos = ((t / 0.12) as usize) % (4 * SEGS);
    let mut out = [0u8; 4];
    out[pos / SEGS] = 1 << (pos % SEGS);
    out
}

/// Маски для коротких статусов на экране часов.
pub fn notice_masks(notice: skynet_core::events::UiNotice) -> [u8; 4] {
    use skynet_core::events::UiNotice;
    match notice {
        UiNotice::Done => [DIGITS[8]; 4],
        UiNotice::Stopped => [0x40; 4], // «----»
        UiNotice::Cleared => [DIGITS[0]; 4],
        UiNotice::Error => [0x79; 4], // «EEEE»
        _ => [0; 4],
    }
}

/// Собирает пути сегментов для тестов.
pub fn clock_paths(profile: &HardwareProfile, dir: &Path) -> HashMap<String, PathBuf> {
    let prefix = profile.clock_dir.rsplit('/').next().unwrap_or("clock");
    let mut out = HashMap::new();
    for d in 0..4 {
        for s in 0..SEGS {
            let name = profile.segment_name(d, s);
            out.insert(
                name.clone(),
                dir.join(format!("{prefix}{name}")).join("brightness"),
            );
        }
    }
    for c in profile.clock_colon {
        out.insert(
            (*c).to_owned(),
            dir.join(format!("{prefix}{c}")).join("brightness"),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use skynet_core::profile::MINI2;

    fn tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for (_, p) in clock_paths(&MINI2, d.path()) {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "0").unwrap();
        }
        d
    }

    fn read(d: &tempfile::TempDir, n: &str) -> u8 {
        let paths = clock_paths(&MINI2, d.path());
        std::fs::read_to_string(&paths[n])
            .unwrap()
            .trim()
            .parse::<u8>()
            .unwrap()
    }

    #[test]
    fn shows_time_on_segments() {
        let d = tree();
        let mut c = Clock::open(&MINI2, d.path(), 1.0);
        assert!(c.enabled());
        c.show_time(12, 34, true).unwrap();
        // «1» = b+c, «2» = a+b+d+e+g, «3» = a+b+c+d+g, «4» = b+c+f+g.
        assert_eq!(read(&d, "A1"), 0);
        assert_ne!(read(&d, "B1"), 0);
        assert_ne!(read(&d, "C1"), 0);
        assert_eq!(read(&d, "A2"), 255);
        assert_eq!(read(&d, "PLUS1"), 255);
    }

    #[test]
    fn timer_shows_minutes_for_long_timers() {
        let d = tree();
        let mut c = Clock::open(&MINI2, d.path(), 1.0);
        c.show_timer(65).unwrap();
        // 65 секунд → 02:00, старшие цифры «0» и «2».
        assert_eq!(read(&d, "A1"), 255);
        assert_eq!(read(&d, "A2"), 255);
        assert_eq!(read(&d, "B2"), 255);
    }

    #[test]
    fn short_timer_shows_seconds() {
        let d = tree();
        let mut c = Clock::open(&MINI2, d.path(), 1.0);
        c.show_timer(42).unwrap();
        // 42 секунды → 00:42.
        assert_eq!(read(&d, "A3"), 0);
        assert_eq!(read(&d, "B3"), 255);
        assert_eq!(read(&d, "A4"), 255);
    }

    #[test]
    fn colon_level_is_smoothly_adjustable() {
        let d = tree();
        let mut c = Clock::open(&MINI2, d.path(), 1.0);
        c.set_levels(&[0; 4], 200, 255).unwrap();
        assert_eq!(read(&d, "PLUS1"), 200);
        c.set_levels(&[0; 4], 40, 255).unwrap();
        assert_eq!(read(&d, "PLUS1"), 40);
        c.set_brightness(0.5);
        c.set_levels(&[digit_mask(8); 4], 0, c.level()).unwrap();
        assert_eq!(read(&d, "A1"), 128);
    }

    #[test]
    fn spinner_moves_over_time() {
        assert_eq!(spinner_masks(0.0)[0].count_ones(), 1);
        assert_eq!(spinner_masks(0.84)[1].count_ones(), 1);
        assert_eq!(
            notice_masks(skynet_core::events::UiNotice::Stopped),
            [0x40; 4]
        );
    }

    #[test]
    fn missing_panel_is_disabled() {
        let d = tempfile::tempdir().unwrap();
        let c = Clock::open(&MINI2, d.path(), 1.0);
        assert!(!c.enabled());
    }
}
