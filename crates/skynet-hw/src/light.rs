//! Датчик освещённости Мини 2.
//!
//! На части ревизий стоит `ltr308als01` (IIO): сырое значение
//! `in_intensity_input` делится на 120, как в прошивке. Если датчика нет,
//! яркость остаётся фиксированной — это не ошибка запуска.

use std::path::{Path, PathBuf};

const IIO_DEVICES: &str = "/sys/bus/iio/devices";
const LTR_SCALE: f32 = 1.0 / 120.0;

pub struct LightSensor {
    path: PathBuf,
    scale: f32,
    last_lux: f32,
}

impl LightSensor {
    /// `override_path` — файл значения или каталог IIO-устройства. Если не
    /// задан, датчик ищется автоматически.
    pub fn open(override_path: Option<&Path>) -> Option<Self> {
        let (path, scale) = match override_path {
            Some(p) if !p.as_os_str().is_empty() => resolve_path(p)?,
            _ => discover()?,
        };
        Some(Self {
            path,
            scale,
            last_lux: 0.0,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&mut self) -> Option<f32> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        let raw: f32 = text.trim().parse().ok()?;
        let lux = (raw * self.scale).clamp(0.0, 100_000.0);
        self.last_lux = lux;
        Some(lux)
    }

    pub fn last_lux(&self) -> f32 {
        self.last_lux
    }
}

/// Ищет IIO-устройство `ltr308` и файл интенсивности.
pub fn discover() -> Option<(PathBuf, f32)> {
    let entries = std::fs::read_dir(IIO_DEVICES).ok()?;
    let mut candidates: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("iio:device"))
        })
        .collect();
    candidates.sort();
    for dir in candidates {
        let name = std::fs::read_to_string(dir.join("name")).unwrap_or_default();
        let name = name.trim().to_ascii_lowercase();
        if name.contains("ltr308") || name.contains("ltr303") || name.contains("ltr329") {
            if let Some(found) = resolve_path(&dir) {
                return Some(found);
            }
        }
    }
    None
}

fn resolve_path(p: &Path) -> Option<(PathBuf, f32)> {
    if p.is_file() {
        let scale = if p
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains("illuminance"))
        {
            1.0
        } else {
            LTR_SCALE
        };
        return Some((p.to_path_buf(), scale));
    }
    for (name, scale) in [
        ("in_intensity_input", LTR_SCALE),
        ("in_illuminance_input", 1.0),
    ] {
        let candidate = p.join(name);
        if candidate.is_file() {
            return Some((candidate, scale));
        }
    }
    None
}

/// Переводит люксы в долю максимальной яркости 0..1 по логарифмической
/// шкале. Ночью не опускается ниже `night_fraction`.
pub fn brightness_factor(lux: f32, night_fraction: f32) -> f32 {
    let night = night_fraction.clamp(0.0, 1.0);
    let f = ((lux.max(0.0) + 1.0).log10() / 4.0).clamp(0.0, 1.0);
    let eased = f * f * (3.0 - 2.0 * f);
    night + (1.0 - night) * eased
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(dir: &Path, name: &str, file: &str, value: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("name"), name).unwrap();
        std::fs::write(dir.join(file), value).unwrap();
    }

    #[test]
    fn reads_raw_intensity_and_scales() {
        let d = tempfile::tempdir().unwrap();
        device(d.path(), "ltr308als01", "in_intensity_input", "2400\n");
        let mut s = LightSensor::open(Some(d.path())).unwrap();
        assert!((s.read().unwrap() - 20.0).abs() < 0.001);
        assert_eq!(s.path(), &d.path().join("in_intensity_input"));
    }

    #[test]
    fn reads_lux_file_without_scaling() {
        let d = tempfile::tempdir().unwrap();
        device(d.path(), "ltr308als01", "in_illuminance_input", "42\n");
        let mut s = LightSensor::open(Some(d.path())).unwrap();
        assert!((s.read().unwrap() - 42.0).abs() < 0.001);
    }

    #[test]
    fn brightness_is_monotonic_and_bounded() {
        let a = brightness_factor(0.0, 0.15);
        let b = brightness_factor(100.0, 0.15);
        let c = brightness_factor(10_000.0, 0.15);
        assert!((a - 0.15).abs() < 0.001);
        assert!(a < b && b < c);
        assert!((c - 1.0).abs() < 0.001);
    }

    #[test]
    fn missing_sensor_is_none() {
        assert!(LightSensor::open(Some(Path::new("/nonexistent/light"))).is_none());
    }
}
