//! Кнопки через evdev и аппаратный мут микрофонов через GPIO.
//! Чтение не меняет настройки устройства.

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::FileTypeExt;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;

pub const KEY_VOLUMEDOWN: u16 = 114;
pub const KEY_VOLUMEUP: u16 = 115;
pub const KEY_MUTE: u16 = 113;
pub const KEY_VOICECOMMAND: u16 = 582;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonEvent {
    Action,
    VolumeUp,
    VolumeDown,
    Mute(bool),
}

/// Ищет evdev-узел по имени в `/proc/bus/input/devices`.
pub fn find_event_node(name: &str, devices: &str) -> Option<PathBuf> {
    let mut matched = false;
    for block in devices.split("\n\n") {
        let mut this_name = "";
        let mut handler = None;
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("N: Name=") {
                this_name = rest.trim().trim_matches('"');
            }
            if let Some(rest) = line.strip_prefix("H: Handlers=") {
                handler = rest
                    .split_whitespace()
                    .find(|h| h.starts_with("event"))
                    .map(str::to_owned);
            }
        }
        if this_name == name {
            matched = true;
            if let Some(h) = handler {
                return Some(PathBuf::from("/dev/input").join(h));
            }
        }
    }
    let _ = matched;
    None
}

#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
const EVENT_LEN: usize = if cfg!(target_arch = "arm") { 16 } else { 24 };
#[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
const EVENT_LEN: usize = 24;

/// Разбирает `struct input_event` текущей архитектуры.
pub fn parse_event(bytes: &[u8]) -> Option<(u16, u16, i32)> {
    if bytes.len() < EVENT_LEN {
        return None;
    }
    let (type_, code, value) = if cfg!(target_pointer_width = "32") {
        (
            u16::from_le_bytes([bytes[8], bytes[9]]),
            u16::from_le_bytes([bytes[10], bytes[11]]),
            i32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
        )
    } else {
        (
            u16::from_le_bytes([bytes[16], bytes[17]]),
            u16::from_le_bytes([bytes[18], bytes[19]]),
            i32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]),
        )
    };
    Some((type_, code, value))
}

/// Запускает поток чтения кнопок. Ошибки только журналируются: без кнопок
/// голосовой помощник продолжает работать.
pub fn start_buttons(device: Option<&str>, action_key: u16, tx: UnboundedSender<ButtonEvent>) {
    let Some(name) = device.map(str::to_owned) else {
        return;
    };
    let devices = std::fs::read_to_string("/proc/bus/input/devices").unwrap_or_default();
    let Some(path) = find_event_node(&name, &devices) else {
        tracing::warn!("кнопки: устройство {name} не найдено");
        return;
    };
    thread::Builder::new()
        .name("skynet-buttons".into())
        .spawn(move || {
            let file = match File::open(&path) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("кнопки: {}: {e}", path.display());
                    return;
                }
            };
            let mut file = file;
            let mut buf = [0u8; 64];
            loop {
                match file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        for chunk in buf[..n].chunks_exact(EVENT_LEN) {
                            let Some((t, code, value)) = parse_event(chunk) else {
                                continue;
                            };
                            if t != 1 || value != 1 {
                                continue;
                            }
                            let ev = if code == action_key || code == KEY_VOICECOMMAND {
                                Some(ButtonEvent::Action)
                            } else if code == KEY_VOLUMEUP {
                                Some(ButtonEvent::VolumeUp)
                            } else if code == KEY_VOLUMEDOWN {
                                Some(ButtonEvent::VolumeDown)
                            } else if code == KEY_MUTE {
                                Some(ButtonEvent::Mute(true))
                            } else {
                                None
                            };
                            if let Some(ev) = ev {
                                if tx.send(ev).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        tracing::warn!("кнопки: чтение: {e}");
                        thread::sleep(Duration::from_millis(500));
                    }
                }
            }
        })
        .ok();
}

/// Опрашивает GPIO мута (активный низкий уровень) и сообщает изменения.
pub fn start_mute(gpio: Option<u32>, tx: UnboundedSender<ButtonEvent>) {
    let Some(gpio) = gpio else {
        return;
    };
    thread::Builder::new()
        .name("skynet-mute".into())
        .spawn(move || {
            let path = PathBuf::from(format!("/sys/class/gpio/gpio{gpio}/value"));
            let mut last: Option<bool> = None;
            loop {
                let muted = match std::fs::read_to_string(&path) {
                    Ok(v) => Some(v.trim() == "0"),
                    Err(_) => None,
                };
                if let Some(m) = muted {
                    if last != Some(m) {
                        last = Some(m);
                        if tx.send(ButtonEvent::Mute(m)).is_err() {
                            return;
                        }
                    }
                }
                thread::sleep(Duration::from_millis(400));
            }
        })
        .ok();
}

/// Проверка, что путь — символьное устройство (диагностика).
pub fn is_char_device(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.file_type().is_char_device())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_node_by_name() {
        let text = r#"I: Bus=0019 Vendor=0000 Product=0000 Version=0000
N: Name="sar0_pad0"
P: Phys=
H: Handlers=kbd event2

I: Bus=0000 Vendor=0000 Product=0000 Version=0000
N: Name="other"
H: Handlers=event3
"#;
        assert_eq!(
            find_event_node("sar0_pad0", text).unwrap(),
            PathBuf::from("/dev/input/event2")
        );
        assert!(find_event_node("missing", text).is_none());
    }

    #[test]
    fn parses_key_event_for_pointer_width() {
        let mut b = vec![0u8; EVENT_LEN];
        let off = if cfg!(target_pointer_width = "32") {
            8
        } else {
            16
        };
        b[off..off + 2].copy_from_slice(&1u16.to_le_bytes());
        b[off + 2..off + 4].copy_from_slice(&KEY_VOICECOMMAND.to_le_bytes());
        b[off + 4..off + 8].copy_from_slice(&1i32.to_le_bytes());
        assert_eq!(parse_event(&b), Some((1, KEY_VOICECOMMAND, 1)));
        assert_eq!(parse_event(&b[..4]), None);
    }
}
