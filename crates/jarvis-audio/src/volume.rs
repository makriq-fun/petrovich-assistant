//! Громкость через `amixer` (та же утилита, что использует прошивка).
//! Команда фиксированная: это не доступ LLM к оболочке.

use std::process::Command;

use anyhow::{bail, Context, Result};
use jarvis_core::ports::{VolumeControl, VolumeStatus};

fn core_err(e: anyhow::Error) -> jarvis_core::Error {
    jarvis_core::Error::Hardware(format!("{e:#}"))
}

#[derive(Debug, Clone)]
pub struct AmixerVolume {
    pcm: String,
    control: String,
    step: i32,
}

impl AmixerVolume {
    pub fn new(pcm: impl Into<String>, step: i32) -> Self {
        Self {
            pcm: pcm.into(),
            control: "Master".into(),
            step: step.clamp(1, 50),
        }
    }

    pub fn step_size(&self) -> i32 {
        self.step
    }

    pub fn with_control(mut self, control: impl Into<String>) -> Self {
        self.control = control.into();
        self
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let out = Command::new("amixer")
            .arg("-D")
            .arg(&self.pcm)
            .args(args)
            .output()
            .with_context(|| format!("запуск amixer -D {}", self.pcm))?;
        if !out.status.success() {
            bail!(
                "amixer {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// Разбирает вывод `amixer sget`: `[42%] [on]`.
pub fn parse_volume(text: &str) -> Option<VolumeStatus> {
    let mut percent = None;
    let mut muted = false;
    for token in text.split_whitespace() {
        if let Some(v) = token.strip_prefix('[').and_then(|t| t.strip_suffix("%]")) {
            if let Ok(n) = v.parse::<i32>() {
                percent = Some(n.clamp(0, 100));
            }
        } else if token == "[off]" {
            muted = true;
        }
    }
    percent.map(|percent| VolumeStatus { percent, muted })
}

impl VolumeControl for AmixerVolume {
    fn get(&self) -> jarvis_core::Result<VolumeStatus> {
        let text = self.run(&["sget", &self.control]).map_err(core_err)?;
        parse_volume(&text)
            .ok_or_else(|| jarvis_core::Error::Hardware("amixer: не найден уровень".into()))
    }

    fn set_percent(&self, percent: i32) -> jarvis_core::Result<VolumeStatus> {
        let percent = percent.clamp(0, 100);
        let arg = format!("{percent}%");
        self.run(&["sset", &self.control, &arg]).map_err(core_err)?;
        self.get()
    }

    fn step(&self, delta: i32) -> jarvis_core::Result<VolumeStatus> {
        let cur = self.get()?;
        let next = (cur.percent + delta).clamp(0, 100);
        if next == cur.percent {
            return Ok(cur);
        }
        self.set_percent(next)
    }
}

/// Шаг по умолчанию из конфигурации.
pub fn step_from_config(step: i32) -> i32 {
    step.clamp(1, 50)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_amixer_playback_output() {
        let text = "Simple mixer control 'Master',0\n  Capabilities: pvolume\n  Playback channels: Front Left - Front Right\n  Limits: Playback 0 - 255\n  Mono:\n  Front Left: Playback 128 [50%] [-6.00dB] [on]\n  Front Right: Playback 128 [50%] [-6.00dB] [on]\n";
        assert_eq!(
            parse_volume(text),
            Some(VolumeStatus {
                percent: 50,
                muted: false
            })
        );
    }

    #[test]
    fn detects_mute() {
        let text = "Front Left: Playback 0 [0%] [-51.00dB] [off]";
        assert_eq!(
            parse_volume(text),
            Some(VolumeStatus {
                percent: 0,
                muted: true
            })
        );
    }

    #[test]
    fn missing_volume_is_none() {
        assert_eq!(parse_volume("no volume here"), None);
    }
}
