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
/// чтобы тесты подменяли её без запуска настоящего BlueZ.
#[derive(Clone)]
pub struct Bluetooth {
    program: PathBuf,
    timeout: Duration,
    #[cfg(test)]
    runner: Option<TestRunner>,
}

#[cfg(test)]
type TestRunner = std::sync::Arc<dyn Fn(&[&str]) -> Result<String> + Send + Sync>;

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
            #[cfg(test)]
            runner: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn fake(runner: impl Fn(&[&str]) -> Result<String> + Send + Sync + 'static) -> Self {
        Self {
            program: PathBuf::from("bluetoothctl"),
            timeout: Duration::from_secs(5),
            runner: Some(std::sync::Arc::new(runner)),
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
        #[cfg(test)]
        if let Some(runner) = &self.runner {
            return runner(args);
        }
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
    use std::sync::{Arc, Mutex};

    /// Подменяет bluetoothctl состоянием в памяти: без файлов и гонок
    /// fork/exec с параллельными тестами.
    fn adapter(powered: bool, discoverable: bool) -> (Bluetooth, Arc<Mutex<(bool, bool)>>) {
        let state = Arc::new(Mutex::new((powered, discoverable)));
        let shared = Arc::clone(&state);
        let bluetooth = Bluetooth::fake(move |args| {
            let mut state = shared.lock().unwrap();
            match args {
                ["show"] => {
                    let (powered, discoverable) = *state;
                    Ok(format!(
                        "Controller 00:11 (public)\n\tAlias: Тест\n\tPowered: {}\n\tDiscoverable: {}\n",
                        if powered { "yes" } else { "no" },
                        if discoverable { "yes" } else { "no" }
                    ))
                }
                ["power", "on"] => {
                    state.0 = true;
                    Ok("Changing power on succeeded".into())
                }
                ["power", "off"] => {
                    state.0 = false;
                    Ok("Changing power off succeeded".into())
                }
                ["discoverable", "on"] => {
                    state.1 = true;
                    Ok("Changing discoverable on succeeded".into())
                }
                ["discoverable", "off"] => {
                    state.1 = false;
                    Ok("Changing discoverable off succeeded".into())
                }
                _ => Ok(format!("Changing {} succeeded", args.join(" "))),
            }
        });
        (bluetooth, state)
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
        let (b, state) = adapter(false, false);
        let text = b.perform(BluetoothAction::On).await.unwrap();
        assert!(text.contains("включён"), "{text}");
        assert!(text.contains("видна для сопряжения"), "{text}");
        assert!(text.contains("Тест"), "{text}");
        assert_eq!(*state.lock().unwrap(), (true, true));
    }

    #[tokio::test]
    async fn off_requires_powered_no() {
        let (b, state) = adapter(true, true);
        let text = b.perform(BluetoothAction::Off).await.unwrap();
        assert_eq!(text, "Bluetooth выключен.");
        assert_eq!(*state.lock().unwrap(), (false, false));
    }

    #[tokio::test]
    async fn status_does_not_change_power() {
        let (b, state) = adapter(true, true);
        let text = b.perform(BluetoothAction::Status).await.unwrap();
        assert!(text.contains("видимость для сопряжения: да"), "{text}");
        assert_eq!(*state.lock().unwrap(), (true, true));
    }
}
