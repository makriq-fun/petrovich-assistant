//! Bluetooth адаптера колонки через BlueZ. Управление идёт закрытым
//! набором действий; произвольных команд и оболочки нет.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::process::Command;

/// Что сделать с адаптером.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BluetoothAction {
    /// Включить, разрешить сопряжение и видимость.
    On,
    /// Выключить адаптер.
    Off,
    /// Только прочитать состояние, ничего не меняя.
    Status,
}

impl BluetoothAction {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim().to_ascii_lowercase().as_str() {
            "on" | "enable" | "включить" => Self::On,
            "off" | "disable" | "выключить" => Self::Off,
            "status" | "state" | "состояние" => Self::Status,
            _ => return None,
        })
    }
}

/// Управление адаптером через `bluetoothctl`. Программа вынесена в поле,
/// чтобы тесты подменяли её скриптом и не трогали настоящий BlueZ.
#[derive(Clone, Debug)]
pub struct Bluetooth {
    program: PathBuf,
    timeout: Duration,
}

impl Default for Bluetooth {
    fn default() -> Self {
        Self::new("bluetoothctl")
    }
}

impl Bluetooth {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            timeout: Duration::from_secs(5),
        }
    }

    pub async fn perform(&self, action: BluetoothAction) -> Result<String> {
        match action {
            BluetoothAction::On => {
                self.run(&["power", "on"]).await?;
                // Pairable и discoverable нужны, чтобы телефон увидел
                // колонку сразу после голосовой команды. Ошибки этих
                // настроек не отменяют уже включённый адаптер.
                let _ = self.run(&["pairable", "on"]).await;
                let _ = self.run(&["discoverable", "on"]).await;
                let state = self.state().await?;
                if !state.powered {
                    bail!("Bluetooth: адаптер не включился");
                }
                let mut out = "Bluetooth включён".to_owned();
                if state.discoverable {
                    out.push_str(", колонка видна для сопряжения");
                } else {
                    out.push_str(", но видимость для сопряжения не включилась");
                }
                if let Some(name) = state.name {
                    out.push_str(&format!(", имя «{name}»"));
                }
                out.push('.');
                Ok(out)
            }
            BluetoothAction::Off => {
                let _ = self.run(&["discoverable", "off"]).await;
                let _ = self.run(&["pairable", "off"]).await;
                self.run(&["power", "off"]).await?;
                if self.state().await.map(|s| s.powered).unwrap_or(false) {
                    bail!("Bluetooth: адаптер не выключился");
                }
                Ok("Bluetooth выключен.".to_owned())
            }
            BluetoothAction::Status => {
                let state = self.state().await?;
                let name = state.name.unwrap_or_else(|| "колонка".to_owned());
                if state.powered {
                    Ok(format!(
                        "Bluetooth включён, имя «{name}», видимость для сопряжения: {}.",
                        if state.discoverable { "да" } else { "нет" }
                    ))
                } else {
                    Ok(format!("Bluetooth выключен, имя «{name}»."))
                }
            }
        }
    }

    async fn state(&self) -> Result<AdapterState> {
        let output = self.run(&["show"]).await?;
        parse_show(&output)
            .ok_or_else(|| anyhow::anyhow!("Bluetooth: bluetoothctl не вернул состояние адаптера"))
    }

    async fn run(&self, args: &[&str]) -> Result<String> {
        let mut command = Command::new(&self.program);
        command.args(args).kill_on_drop(true);
        let output = tokio::time::timeout(self.timeout, command.output())
            .await
            .with_context(|| format!("{} {}: тайм-аут", self.program.display(), args.join(" ")))?
            .with_context(|| format!("запуск {}", self.program.display()))?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if !output.status.success() {
            let detail = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            bail!(
                "{} {}: код {:?}{}",
                self.program.display(),
                args.join(" "),
                output.status.code(),
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            );
        }
        // bluetoothctl умеет печатать ошибку и выходить с кодом 0, поэтому
        // проверяем ещё и текст.
        let all = format!("{stdout}\n{stderr}").to_ascii_lowercase();
        if all.contains("failed") || all.contains("not available") {
            bail!(
                "{} {}: {}",
                self.program.display(),
                args.join(" "),
                all.trim()
            );
        }
        Ok(stdout)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AdapterState {
    powered: bool,
    discoverable: bool,
    name: Option<String>,
}

fn parse_show(output: &str) -> Option<AdapterState> {
    if !output.contains("Controller ") {
        return None;
    }
    let field = |key: &str| {
        output.lines().find_map(|line| {
            let line = line.trim();
            let (k, v) = line.split_once(':')?;
            (k.trim() == key).then(|| v.trim().to_owned())
        })
    };
    let powered = field("Powered")?.eq_ignore_ascii_case("yes");
    let discoverable = field("Discoverable").is_some_and(|v| v.eq_ignore_ascii_case("yes"));
    let name = field("Alias").or_else(|| field("Name"));
    Some(AdapterState {
        powered,
        discoverable,
        name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn stub(dir: &std::path::Path, body: &str) -> Bluetooth {
        let path = dir.join("bluetoothctl");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        Bluetooth::new(path)
    }

    #[test]
    fn parses_show_output() {
        let state = parse_show(
            "Controller 3C:0B:4F:AC:23:4F (public)\n\
             \tName: Yandex.Mini2\n\
             \tAlias: Станция Max\n\
             \tPowered: yes\n\
             \tDiscoverable: no\n",
        )
        .unwrap();
        assert!(state.powered);
        assert!(!state.discoverable);
        assert_eq!(state.name.as_deref(), Some("Станция Max"));
        assert!(parse_show("No default controller available\n").is_none());
    }

    #[tokio::test]
    async fn on_enables_discoverable_and_reports_name() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls");
        let b = stub(
            dir.path(),
            &format!(
                "echo \"$*\" >> '{}'\n\
                 if [ \"$1\" = show ]; then\n\
                   printf 'Controller 00:11 (public)\\n\\tAlias: Тест\\n\\tPowered: yes\\n\\tDiscoverable: no\\n'\n\
                 else\n\
                   echo \"Changing $1 $2 succeeded\"\n\
                 fi",
                log.display()
            ),
        );
        let text = b.perform(BluetoothAction::On).await.unwrap();
        assert!(text.contains("включён"), "{text}");
        assert!(text.contains("Тест"), "{text}");
        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(calls.contains("power on"), "{calls}");
        assert!(calls.contains("discoverable on"), "{calls}");
    }

    #[tokio::test]
    async fn off_requires_powered_no() {
        let dir = tempfile::tempdir().unwrap();
        let b = stub(
            dir.path(),
            "if [ \"$1\" = show ]; then\n\
               printf 'Controller 00:11 (public)\\n\\tPowered: no\\n\\tDiscoverable: no\\n'\n\
             else\n\
               echo \"Changing $1 $2 succeeded\"\n\
             fi",
        );
        let text = b.perform(BluetoothAction::Off).await.unwrap();
        assert_eq!(text, "Bluetooth выключен.");
    }

    #[tokio::test]
    async fn status_does_not_change_power() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls");
        let b = stub(
            dir.path(),
            &format!(
                "echo \"$*\" >> '{}'\n\
                 printf 'Controller 00:11 (public)\\n\\tAlias: Тест\\n\\tPowered: yes\\n\\tDiscoverable: yes\\n'",
                log.display()
            ),
        );
        let text = b.perform(BluetoothAction::Status).await.unwrap();
        assert!(text.contains("видимость для сопряжения: да"), "{text}");
        assert_eq!(std::fs::read_to_string(&log).unwrap().trim(), "show");
    }
}
