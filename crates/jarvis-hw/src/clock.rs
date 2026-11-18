//! Часы Мини 2: по одному sysfs-светодиоду на сегмент,
//! `mini2:white:clock{A..G}{1..4}`, двоеточие PLUS1/PLUS2.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use jarvis_core::profile::HardwareProfile;

pub const DEFAULT_LED_DIR: &str = "/sys/class/leds/";

const DIGITS: [u8; 10] = [0x3f, 0x06, 0x5b, 0x4f, 0x66, 0x6d, 0x7d, 0x07, 0x7f, 0x6f];
const SEGS: usize = 7;

pub struct Clock {
    segments: Vec<File>,
    colon: Vec<File>,
    last: Vec<u8>,
    level: u8,
    enabled: bool,
}

impl Clock {
    pub fn open(profile: &HardwareProfile, dir: &Path, brightness: f32) -> Self {
        let mut segments = Vec::new();
        let mut paths: Vec<PathBuf> = Vec::new();
        for digit in 0..4 {
            for seg in 0..SEGS {
                paths.push(
                    dir.join(profile.segment_name(digit, seg))
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
            let p = dir.join(name).join("brightness");
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
            let p = dir.join(name).join("brightness");
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
        }
    }

    fn disabled() -> Self {
        Self {
            segments: Vec::new(),
            colon: Vec::new(),
            last: Vec::new(),
            level: 0,
            enabled: false,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set(&mut self, bytes: &[u8; 4], colon_on: bool) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let mut values = Vec::with_capacity(self.segments.len() + self.colon.len());
        for (digit, b) in bytes.iter().enumerate() {
            let mask = DIGITS[(*b as usize).min(9)];
            for seg in 0..SEGS {
                values.push(if mask >> seg & 1 != 0 { self.level } else { 0 });
            }
            let _ = digit;
        }
        for _ in &self.colon {
            values.push(if colon_on { self.level } else { 0 });
        }
        for (i, f) in self.segments.iter_mut().enumerate() {
            if values[i] != self.last[i] {
                store(f, values[i].to_string().as_bytes())?;
                self.last[i] = values[i];
            }
        }
        let off = self.segments.len();
        for (j, f) in self.colon.iter_mut().enumerate() {
            let v = values[off + j];
            if v != self.last[off + j] {
                store(f, v.to_string().as_bytes())?;
                self.last[off + j] = v;
            }
        }
        Ok(())
    }

    /// Показывает HH:MM.
    pub fn show_time(&mut self, hour: u32, minute: u32, colon: bool) -> Result<()> {
        let bytes = [
            ((hour / 10) % 10) as u8,
            (hour % 10) as u8,
            ((minute / 10) % 10) as u8,
            (minute % 10) as u8,
        ];
        self.set(&bytes, colon)
    }

    /// Показывает MM:SS для таймера.
    pub fn show_timer(&mut self, seconds: u64) -> Result<()> {
        let s = seconds.min(99 * 60 + 59);
        let bytes = [
            ((s / 60) / 10) as u8,
            ((s / 60) % 10) as u8,
            ((s % 60) / 10) as u8,
            (s % 60) as u8,
        ];
        self.set(&bytes, true)
    }

    pub fn all_off(&mut self) -> Result<()> {
        self.set(&[0; 4], false)
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

/// Собирает пути сегментов для тестов.
pub fn clock_paths(profile: &HardwareProfile, dir: &Path) -> HashMap<String, PathBuf> {
    let mut out = HashMap::new();
    for d in 0..4 {
        for s in 0..SEGS {
            let name = profile.segment_name(d, s);
            out.insert(name.clone(), dir.join(name).join("brightness"));
        }
    }
    for c in profile.clock_colon {
        out.insert((*c).to_owned(), dir.join(c).join("brightness"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_core::profile::MINI2;

    fn tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for (_, p) in clock_paths(&MINI2, d.path()) {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "0").unwrap();
        }
        d
    }

    #[test]
    fn shows_time_on_segments() {
        let d = tree();
        let mut c = Clock::open(&MINI2, d.path(), 1.0);
        assert!(c.enabled());
        c.show_time(12, 34, true).unwrap();
        let read = |n: &str| {
            std::fs::read_to_string(d.path().join(n).join("brightness"))
                .unwrap()
                .trim()
                .parse::<u8>()
                .unwrap()
        };
        // «1» = b+c, «2» = a+b+d+e+g, «3» = a+b+c+d+g, «4» = b+c+f+g.
        assert_eq!(read("A1"), 0);
        assert_ne!(read("B1"), 0);
        assert_ne!(read("C1"), 0);
        assert_eq!(read("A2"), 255);
        assert_eq!(read("PLUS1"), 255);
    }

    #[test]
    fn timer_shows_mmss() {
        let d = tree();
        let mut c = Clock::open(&MINI2, d.path(), 0.5);
        c.show_timer(65).unwrap();
        assert_eq!(digit_mask(1), 0x06);
        assert_eq!(digit_mask(6), 0x7d);
    }

    #[test]
    fn missing_panel_is_disabled() {
        let d = tempfile::tempdir().unwrap();
        let c = Clock::open(&MINI2, d.path(), 1.0);
        assert!(!c.enabled());
    }
}
