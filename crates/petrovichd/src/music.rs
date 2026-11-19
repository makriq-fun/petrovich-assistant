//! Музыка через Spotiya или другую настроенную команду.

use std::process::Command;

use anyhow::{bail, Context, Result};
use petrovich_core::ports::MusicControl;

#[derive(Clone)]
pub struct CommandMusic {
    argv: Vec<String>,
}

impl CommandMusic {
    pub fn new(argv: Vec<String>) -> Option<Self> {
        (!argv.is_empty()).then_some(Self { argv })
    }

    fn run(&self, action: &str) -> Result<String> {
        self.run_extra(action, None)
    }

    fn run_extra(&self, action: &str, extra: Option<&str>) -> Result<String> {
        let template = self.argv.iter().any(|a| a.contains("{action}"));
        let mut cmd = Command::new(&self.argv[0]);
        if template {
            cmd.args(self.argv[1..].iter().map(|a| a.replace("{action}", action)));
        } else {
            cmd.args(&self.argv[1..]).arg(action);
        }
        if let Some(extra) = extra {
            cmd.arg(extra);
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
    fn play(&self) -> petrovich_core::Result<()> {
        self.run("play").map(|_| ()).map_err(err)
    }
    fn play_query(&self, query: &str) -> petrovich_core::Result<()> {
        self.run_extra("play", Some(query)).map(|_| ()).map_err(err)
    }
    fn pause(&self) -> petrovich_core::Result<()> {
        self.run("pause").map(|_| ()).map_err(err)
    }
    fn resume(&self) -> petrovich_core::Result<()> {
        self.run("resume").map(|_| ()).map_err(err)
    }
    fn next(&self) -> petrovich_core::Result<()> {
        self.run("next").map(|_| ()).map_err(err)
    }
    fn previous(&self) -> petrovich_core::Result<()> {
        self.run("previous").map(|_| ()).map_err(err)
    }
    fn status(&self) -> petrovich_core::Result<String> {
        let s = self.run("status").map_err(err)?;
        Ok(format_status(&s))
    }
}

/// Spotya отдаёт большой JSON. В LLM отправляем только название, исполнителя
/// и состояние: иначе ответ «что за трек» упирается в лимит токенов.
fn format_status(raw: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw.trim()) else {
        return if raw.trim().is_empty() {
            "Нет данных.".into()
        } else {
            raw.trim().to_owned()
        };
    };
    let text = |key: &str| v.get(key).and_then(|x| x.as_str()).unwrap_or("").trim();
    let error = text("error");
    if !error.is_empty() {
        return format!("Spotya: {error}");
    }
    let title = text("title");
    let artist = text("artist");
    let album = text("album");
    if title.is_empty() {
        return if v.get("paused").and_then(|x| x.as_bool()) == Some(true) {
            "Пауза.".into()
        } else {
            "Ничего не играет.".into()
        };
    }
    let mut out = String::new();
    if v.get("paused").and_then(|x| x.as_bool()) == Some(true) {
        out.push_str("Пауза: ");
    }
    out.push_str(title);
    if !artist.is_empty() {
        out.push_str(" — ");
        out.push_str(artist);
    }
    if !album.is_empty() {
        out.push_str(&format!(" (альбом «{album}»)"));
    }
    out
}

fn err(e: anyhow::Error) -> petrovich_core::Error {
    petrovich_core::Error::Hardware(format!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_action_and_query() {
        let m = CommandMusic::new(vec![
            "sh".into(),
            "-c".into(),
            "printf '%s %s' \"$1\" \"$2\"".into(),
            "sh".into(),
            "--{action}".into(),
        ])
        .unwrap();
        assert_eq!(m.run_extra("play", Some("jazz")).unwrap(), "--play jazz");
        assert_eq!(m.run("status").unwrap(), "--status");
    }

    #[test]
    fn formats_track_status() {
        let raw = r#"{"active":true,"paused":false,"title":"Армавир","artist":"Группа","album":"Юг","error":"","volume":50}"#;
        assert_eq!(format_status(raw), "Армавир — Группа (альбом «Юг»)");
        let raw = r#"{"active":true,"paused":true,"title":"Армавир","artist":"","album":""}"#;
        assert_eq!(format_status(raw), "Пауза: Армавир");
        assert_eq!(
            format_status(r#"{"active":false,"title":""}"#),
            "Ничего не играет."
        );
        assert_eq!(format_status(r#"{"error":"нет сети"}"#), "Spotya: нет сети");
    }

    #[test]
    fn appends_action_without_template() {
        let m = CommandMusic::new(vec![
            "sh".into(),
            "-c".into(),
            "printf '%s' \"$1\"".into(),
            "sh".into(),
        ])
        .unwrap();
        assert_eq!(m.run("pause").unwrap(), "pause");
    }
}
