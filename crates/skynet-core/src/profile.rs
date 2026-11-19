//! Таблица моделей станций. Вся аппаратная специфика — данные, а не ветвления
//! в логике: новая модель добавляется записью в [`PROFILES`].

use std::path::Path;

#[derive(Clone, Copy, Debug)]
pub struct HardwareProfile {
    pub codename: &'static str,
    pub model_name: &'static str,
    pub capture_channels: u32,
    pub mic_channels: &'static [u32],
    pub reference_channels: &'static [u32],
    pub vqe_preset: &'static str,
    pub led_prefix: &'static str,
    pub led_count: usize,
    pub clock_dir: &'static str,
    pub clock_seg_base: char,
    pub clock_pos_base: i32,
    pub clock_colon: &'static [&'static str],
    pub clock_aux: &'static [&'static str],
    pub input_device: &'static str,
    pub mute_gpio: Option<u32>,
    pub action_key: u16,
    pub supported: bool,
    pub note: &'static str,
}

impl HardwareProfile {
    pub fn led_names(&self) -> Vec<[String; 3]> {
        (0..self.led_count)
            .map(|i| {
                let idx = i + 1;
                ["red", "green", "blue"].map(|c| format!("{}:{}:ring{idx}", self.led_prefix, c))
            })
            .collect()
    }

    pub fn clock_dir_path(&self) -> Option<String> {
        Some(self.clock_dir.to_owned())
    }

    pub fn clock_path(&self, name: &str) -> String {
        format!("{}{name}/brightness", self.clock_dir)
    }

    pub fn segment_name(&self, digit: usize, seg: usize) -> String {
        let letter = char::from_u32(self.clock_seg_base as u32 + seg as u32).unwrap_or('?');
        format!("{letter}{}", self.clock_pos_base + digit as i32)
    }
}

/// Станция Мини 2: 4 микрофона + 2 канала обратной связи, 6 светодиодов
/// кольца через sysfs, часы из 28 сегментов (на части ревизий отсутствуют).
pub static MINI2: HardwareProfile = HardwareProfile {
    codename: "station-mini2",
    model_name: "Яндекс Станция Мини 2",
    capture_channels: 6,
    mic_channels: &[0, 1, 2, 3],
    reference_channels: &[4, 5],
    vqe_preset: "yandexmini_2",
    led_prefix: "mini2",
    led_count: 6,
    clock_dir: "/sys/class/leds/mini2:white:clock",
    clock_seg_base: 'A',
    clock_pos_base: 1,
    clock_colon: &["PLUS1", "PLUS2"],
    clock_aux: &["DEGREE", "MINUS"],
    input_device: "sar0_pad0",
    mute_gpio: Some(414),
    action_key: 582,
    supported: true,
    note: "Проверяемая модель проекта",
};

pub static PROFILES: &[HardwareProfile] = &[MINI2];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProfileError {
    #[error("не удалось определить модель: {0}")]
    Detect(String),
    #[error("модель {0} ещё не поддержана: {1}")]
    Unsupported(String, &'static str),
    #[error("неизвестная модель {0}")]
    Unknown(String),
}

/// Разбирает содержимое `/proc/device-tree/compatible`.
pub fn from_compatible(bytes: &[u8]) -> Result<&'static HardwareProfile, ProfileError> {
    let text = String::from_utf8_lossy(bytes);
    for item in text.split('\0') {
        if let Some(code) = item.trim().strip_prefix("yandex,") {
            if let Some(p) = PROFILES.iter().find(|p| p.codename == code) {
                if !p.supported {
                    return Err(ProfileError::Unsupported(code.to_owned(), p.note));
                }
                return Ok(p);
            }
            return Err(ProfileError::Unknown(code.to_owned()));
        }
    }
    Err(ProfileError::Detect(
        "в device-tree нет строки yandex,<модель>".into(),
    ))
}

pub fn detect() -> Result<&'static HardwareProfile, ProfileError> {
    if let Ok(code) = std::env::var("SKYNET_HARDWARE_MODEL") {
        return by_name(&code);
    }
    let path = Path::new("/proc/device-tree/compatible");
    let bytes = std::fs::read(path)
        .map_err(|e| ProfileError::Detect(format!("{}: {e}", path.display())))?;
    from_compatible(&bytes)
}

pub fn by_name(name: &str) -> Result<&'static HardwareProfile, ProfileError> {
    let name = name.trim();
    let name = name.strip_prefix("yandex,").unwrap_or(name);
    match PROFILES.iter().find(|p| p.codename == name) {
        Some(p) if p.supported => Ok(p),
        Some(p) => Err(ProfileError::Unsupported(name.to_owned(), p.note)),
        None => Err(ProfileError::Unknown(name.to_owned())),
    }
}

/// Выбор профиля с учётом `hardware.model = "auto"`.
pub fn resolve(model: &str) -> Result<&'static HardwareProfile, ProfileError> {
    if model.eq_ignore_ascii_case("auto") {
        detect()
    } else {
        by_name(model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mini2_compatible() {
        let p = from_compatible(b"yandex,station-mini2\0amlogic,axg\0").unwrap();
        assert_eq!(p.codename, "station-mini2");
        assert_eq!(p.led_count, 6);
    }

    #[test]
    fn rejects_unknown_and_reports_known_unsupported() {
        assert!(matches!(
            from_compatible(b"yandex,unknown\0"),
            Err(ProfileError::Unknown(_))
        ));
        assert!(matches!(
            from_compatible(b"amlogic,axg\0"),
            Err(ProfileError::Detect(_))
        ));
    }

    #[test]
    fn led_and_clock_names_are_table_driven() {
        let p = &MINI2;
        assert_eq!(
            p.led_names()[0],
            ["mini2:red:ring1", "mini2:green:ring1", "mini2:blue:ring1"]
        );
        assert_eq!(p.segment_name(0, 0), "A1");
        assert_eq!(p.segment_name(3, 6), "G4");
        assert_eq!(
            p.clock_path("PLUS1"),
            "/sys/class/leds/mini2:white:clockPLUS1/brightness"
        );
    }
}
