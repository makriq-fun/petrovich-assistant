//! Музыка через Spotiya или другую настроенную команду.

use std::process::Command;

use anyhow::{bail, Context, Result};
use jarvis_core::ports::MusicControl;

#[derive(Clone)]
pub struct CommandMusic {
    argv: Vec<String>,
}

impl CommandMusic {
    pub fn new(argv: Vec<String>) -> Option<Self> {
        (!argv.is_empty()).then_some(Self { argv })
    }

    fn run(&self, action: &str) -> Result<String> {
        let template = self.argv.iter().any(|a| a.contains("{action}"));
        let mut cmd = Command::new(&self.argv[0]);
        if template {
            cmd.args(self.argv[1..].iter().map(|a| a.replace("{action}", action)));
        } else {
            cmd.args(&self.argv[1..]).arg(action);
        }
        let out = cmd
            .output()
            .with_context(|| format!("музыка: запуск {}", self.argv[0]))?;
        if !out.status.success() {
            bail!(
                "музыка ({action}) вернула {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }
}

impl MusicControl for CommandMusic {
    fn play(&self) -> jarvis_core::Result<()> {
        self.run("play").map(|_| ()).map_err(err)
    }
    fn pause(&self) -> jarvis_core::Result<()> {
        self.run("pause").map(|_| ()).map_err(err)
    }
    fn resume(&self) -> jarvis_core::Result<()> {
        self.run("resume").map(|_| ()).map_err(err)
    }
    fn next(&self) -> jarvis_core::Result<()> {
        self.run("next").map(|_| ()).map_err(err)
    }
    fn previous(&self) -> jarvis_core::Result<()> {
        self.run("previous").map(|_| ()).map_err(err)
    }
    fn status(&self) -> jarvis_core::Result<String> {
        let s = self.run("status").map_err(err)?;
        Ok(if s.is_empty() {
            "Нет данных.".into()
        } else {
            s
        })
    }
}

fn err(e: anyhow::Error) -> jarvis_core::Error {
    jarvis_core::Error::Hardware(format!("{e:#}"))
}
