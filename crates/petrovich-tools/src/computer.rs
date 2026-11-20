//! Управление домашним ПК. Включение и пробуждение — magic packet (WOL),
//! остальные действия — одна команда на ограниченный SSH-endpoint ПК.
//! Свободных команд, оболочки и произвольных аргументов нет: endpoint
//! принимает только закрытый список действий.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use petrovich_core::config::{parse_mac, ComputerConfig};
use tokio::net::UdpSocket;
use tokio::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComputerAction {
    PowerOn,
    PowerOff,
    Sleep,
    Wake,
    Lock,
    CoffeeOn,
    CoffeeOff,
    Status,
}

impl ComputerAction {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "power_on" => Self::PowerOn,
            "power_off" => Self::PowerOff,
            "sleep" => Self::Sleep,
            "wake" => Self::Wake,
            "lock" => Self::Lock,
            "coffee_on" => Self::CoffeeOn,
            "coffee_off" => Self::CoffeeOff,
            "status" => Self::Status,
            _ => return None,
        })
    }
}

#[derive(Clone)]
pub struct Computer {
    cfg: ComputerConfig,
    mac: [u8; 6],
    broadcast: Ipv4Addr,
}

impl Computer {
    pub fn new(cfg: ComputerConfig) -> Result<Self> {
        let mac =
            parse_mac(&cfg.mac).with_context(|| format!("computer: неверный MAC {}", cfg.mac))?;
        let broadcast = cfg
            .broadcast
            .parse::<Ipv4Addr>()
            .with_context(|| format!("computer: неверный broadcast {}", cfg.broadcast))?;
        Ok(Self {
            cfg,
            mac,
            broadcast,
        })
    }

    pub fn config(&self) -> &ComputerConfig {
        &self.cfg
    }

    /// Выполняет действие и возвращает честный текст для LLM.
    pub async fn perform(&self, action: ComputerAction) -> Result<String> {
        match action {
            ComputerAction::PowerOn | ComputerAction::Wake => {
                self.wake().await?;
                Ok(if action == ComputerAction::Wake {
                    format!(
                        "Отправил magic packet на {}: компьютер должен проснуться.",
                        self.cfg.mac
                    )
                } else {
                    format!(
                        "Отправил magic packet на {}: компьютер должен включиться.",
                        self.cfg.mac
                    )
                })
            }
            ComputerAction::PowerOff => {
                self.remote("poweroff").await?;
                Ok("Команда выключения отправлена.".to_owned())
            }
            ComputerAction::Sleep => {
                self.remote("suspend").await?;
                Ok("Команда сна отправлена.".to_owned())
            }
            ComputerAction::Lock => {
                self.remote("lock").await?;
                Ok("Компьютер заблокирован.".to_owned())
            }
            ComputerAction::CoffeeOn => {
                self.remote("coffee-on").await?;
                Ok("Режим кофе включён: компьютер не будет засыпать.".to_owned())
            }
            ComputerAction::CoffeeOff => {
                self.remote("coffee-off").await?;
                Ok("Режим кофе выключен.".to_owned())
            }
            ComputerAction::Status => {
                self.remote("probe").await?;
                Ok("ПК на связи и принимает команды.".to_owned())
            }
        }
    }

    /// Отправляет magic packet трижды: Wi-Fi и коммутаторы теряют первый
    /// широковещательный пакет.
    pub async fn wake(&self) -> Result<()> {
        let target = SocketAddr::new(IpAddr::V4(self.broadcast), self.cfg.wake_port);
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .await
            .context("computer: не удалось открыть UDP-сокет")?;
        socket
            .set_broadcast(true)
            .context("computer: не удалось разрешить broadcast")?;
        let packet = magic_packet(&self.mac);
        for i in 0..3 {
            socket.send_to(&packet, target).await.with_context(|| {
                format!("computer: не удалось отправить magic packet на {target}")
            })?;
            if i < 2 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        Ok(())
    }

    /// Проверка канала без побочных действий.
    pub async fn probe(&self) -> Result<String> {
        self.remote("probe").await
    }

    async fn remote(&self, action: &str) -> Result<String> {
        let timeout = Duration::from_millis(self.cfg.timeout_ms);
        let mut command = Command::new(&self.cfg.ssh_bin);
        command
            .arg("-T")
            .arg("-p")
            .arg(self.cfg.ssh_port.to_string())
            .arg("-i")
            .arg(&self.cfg.key)
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("StrictHostKeyChecking=yes")
            .arg("-I")
            .arg(timeout.as_secs().clamp(1, 60).to_string())
            .arg(format!("root@{}", self.cfg.host))
            .arg(action)
            .env("HOME", &self.cfg.home)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null());
        let output = match tokio::time::timeout(timeout, command.output()).await {
            Ok(result) => result
                .with_context(|| format!("computer: не удалось запустить {}", self.cfg.ssh_bin))?,
            Err(_) => bail!("computer: ПК не ответил за {} мс", self.cfg.timeout_ms),
        };
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if !output.status.success() {
            let detail = if stderr.is_empty() { &stdout } else { &stderr };
            let detail = detail.chars().take(300).collect::<String>();
            if detail.is_empty() {
                bail!("computer: {action} завершилось с {}", output.status);
            }
            bail!(
                "computer: {action} завершилось с {}: {detail}",
                output.status
            );
        }
        Ok(stdout)
    }
}

/// 6 байт 0xff и MAC шестнадцать раз.
pub fn magic_packet(mac: &[u8; 6]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(6 + 16 * mac.len());
    packet.extend_from_slice(&[0xff; 6]);
    for _ in 0..16 {
        packet.extend_from_slice(mac);
    }
    packet
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn config(dir: &std::path::Path, script: &str) -> ComputerConfig {
        let bin = dir.join("dbclient");
        std::fs::write(&bin, script).unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();
        ComputerConfig {
            host: "192.168.3.2".into(),
            mac: "fc:34:97:17:d1:00".into(),
            broadcast: "192.168.3.255".into(),
            wake_port: 9,
            ssh_port: 2222,
            ssh_bin: bin.display().to_string(),
            key: dir.join("client-key").display().to_string(),
            home: dir.display().to_string(),
            timeout_ms: 2_000,
        }
    }

    #[test]
    fn magic_packet_is_six_ff_and_sixteen_macs() {
        let packet = magic_packet(&[0xfc, 0x34, 0x97, 0x17, 0xd1, 0x00]);
        assert_eq!(packet.len(), 6 + 16 * 6);
        assert_eq!(&packet[..6], &[0xff; 6]);
        for chunk in packet[6..].chunks(6) {
            assert_eq!(chunk, [0xfc, 0x34, 0x97, 0x17, 0xd1, 0x00]);
        }
    }

    #[tokio::test]
    async fn remote_actions_use_the_closed_command_set() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("args");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\necho alice-control-ready\n",
            record.display()
        );
        let pc = Computer::new(config(dir.path(), &script)).unwrap();
        for (action, expected) in [
            (ComputerAction::Lock, "lock"),
            (ComputerAction::Sleep, "suspend"),
            (ComputerAction::PowerOff, "poweroff"),
            (ComputerAction::CoffeeOn, "coffee-on"),
            (ComputerAction::CoffeeOff, "coffee-off"),
            (ComputerAction::Status, "probe"),
        ] {
            pc.perform(action).await.unwrap();
            let args = std::fs::read_to_string(&record).unwrap();
            assert_eq!(args.lines().last(), Some(expected));
            assert!(args.contains("root@192.168.3.2"));
            assert!(args.contains("-p\n2222"));
        }
    }

    #[tokio::test]
    async fn remote_failure_is_reported_not_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let pc = Computer::new(config(
            dir.path(),
            "#!/bin/sh\necho 'Command rejected' >&2\nexit 3\n",
        ))
        .unwrap();
        let err = pc.perform(ComputerAction::PowerOff).await.unwrap_err();
        assert!(err.to_string().contains("Command rejected"), "{err}");
    }

    #[tokio::test]
    async fn unknown_action_is_rejected() {
        assert!(ComputerAction::parse("shell").is_none());
        assert_eq!(
            ComputerAction::parse("power_off"),
            Some(ComputerAction::PowerOff)
        );
    }
}
