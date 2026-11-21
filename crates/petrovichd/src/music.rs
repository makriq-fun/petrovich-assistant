//! Музыка через Spotiya, встроенный Spotify или другую настроенную команду.

use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

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
    fn is_playing(&self) -> petrovich_core::Result<bool> {
        let s = self.run("status").map_err(err)?;
        Ok(parse_playing(&s))
    }
}

/// Музыкальный backend, который появляется позже. Spotify поднимается в фоне:
/// прокси (hysteria2) может стартовать медленнее демона. До готовности
/// инструмент `music` остаётся в списке и отвечает причиной, а не исчезает.
pub struct DeferredMusic {
    inner: RwLock<Option<Arc<dyn MusicControl>>>,
    hint: &'static str,
    duck_depth: AtomicU32,
}

impl DeferredMusic {
    pub fn new(hint: &'static str) -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(None),
            hint,
            duck_depth: AtomicU32::new(0),
        })
    }

    /// Подключает готовый backend. Если ассистент уже говорил и музыка была
    /// приглушена, приглушение переносится на новый backend.
    pub fn install(&self, music: Arc<dyn MusicControl>) {
        for _ in 0..self.duck_depth.load(Ordering::Relaxed) {
            music.duck();
        }
        let mut slot = self.inner.write().unwrap_or_else(|e| e.into_inner());
        *slot = Some(music);
    }

    fn backend(&self) -> Option<Arc<dyn MusicControl>> {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn not_ready(&self) -> petrovich_core::Error {
        petrovich_core::Error::External(self.hint.to_owned())
    }

    fn call<T>(
        &self,
        f: impl FnOnce(&dyn MusicControl) -> petrovich_core::Result<T>,
    ) -> petrovich_core::Result<T> {
        match self.backend() {
            Some(music) => f(music.as_ref()),
            None => Err(self.not_ready()),
        }
    }
}

#[async_trait::async_trait]
impl MusicControl for DeferredMusic {
    fn play(&self) -> petrovich_core::Result<()> {
        self.call(|m| m.play())
    }

    fn play_query(&self, query: &str) -> petrovich_core::Result<()> {
        self.call(|m| m.play_query(query))
    }

    async fn play_search(&self, query: &str) -> petrovich_core::Result<String> {
        match self.backend() {
            Some(music) => music.play_search(query).await,
            None => Err(self.not_ready()),
        }
    }

    async fn liked(&self) -> petrovich_core::Result<String> {
        match self.backend() {
            Some(music) => music.liked().await,
            None => Err(self.not_ready()),
        }
    }

    fn pause(&self) -> petrovich_core::Result<()> {
        self.call(|m| m.pause())
    }

    fn resume(&self) -> petrovich_core::Result<()> {
        self.call(|m| m.resume())
    }

    async fn resume_checked(&self) -> petrovich_core::Result<()> {
        match self.backend() {
            Some(music) => music.resume_checked().await,
            None => Err(self.not_ready()),
        }
    }

    fn next(&self) -> petrovich_core::Result<()> {
        self.call(|m| m.next())
    }

    fn previous(&self) -> petrovich_core::Result<()> {
        self.call(|m| m.previous())
    }

    fn status(&self) -> petrovich_core::Result<String> {
        self.call(|m| m.status())
    }

    fn is_playing(&self) -> petrovich_core::Result<bool> {
        match self.backend() {
            Some(music) => music.is_playing(),
            None => Ok(false),
        }
    }

    fn seek_by(&self, delta_ms: i64) -> petrovich_core::Result<()> {
        self.call(|m| m.seek_by(delta_ms))
    }

    fn seek_to(&self, position_ms: u64) -> petrovich_core::Result<()> {
        self.call(|m| m.seek_to(position_ms))
    }

    async fn queue(&self, query: &str) -> petrovich_core::Result<String> {
        match self.backend() {
            Some(music) => music.queue(query).await,
            None => Err(self.not_ready()),
        }
    }

    fn sleep_timer(&self, minutes: u64) -> petrovich_core::Result<()> {
        self.call(|m| m.sleep_timer(minutes))
    }

    async fn resume_saved(&self) -> petrovich_core::Result<String> {
        match self.backend() {
            Some(music) => music.resume_saved().await,
            None => Err(self.not_ready()),
        }
    }

    fn duck(&self) {
        self.duck_depth.fetch_add(1, Ordering::Relaxed);
        if let Some(music) = self.backend() {
            music.duck();
        }
    }

    fn unduck(&self) {
        // CAS вместо `fetch_update`: он есть с 1.45, а переименованный
        // `try_update` требует свежий компилятор при MSRV 1.85.
        let mut cur = self.duck_depth.load(Ordering::Relaxed);
        loop {
            if cur == 0 {
                return;
            }
            match self.duck_depth.compare_exchange_weak(
                cur,
                cur - 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
        if let Some(music) = self.backend() {
            music.unduck();
        }
    }

    fn position(&self) -> petrovich_core::Result<Option<(u64, u64)>> {
        match self.backend() {
            Some(music) => music.position(),
            None => Ok(None),
        }
    }
}

/// Spotya: `active`/`paused`/`title`. Пустой title или пауза — не «играет».
fn parse_playing(raw: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw.trim()) else {
        return false;
    };
    if !v
        .get("error")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .is_empty()
    {
        return false;
    }
    let paused = v.get("paused").and_then(|x| x.as_bool()) == Some(true);
    let title = v.get("title").and_then(|x| x.as_str()).unwrap_or("").trim();
    let active = v
        .get("active")
        .and_then(|x| x.as_bool())
        .unwrap_or(!title.is_empty());
    active && !paused && !title.is_empty()
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
    fn detects_only_audible_music() {
        assert!(parse_playing(
            r#"{"active":true,"paused":false,"title":"Армавир","error":""}"#
        ));
        assert!(!parse_playing(
            r#"{"active":true,"paused":true,"title":"Армавир"}"#
        ));
        assert!(!parse_playing(r#"{"active":false,"title":""}"#));
        assert!(!parse_playing(r#"{"error":"нет сети"}"#));
        assert!(!parse_playing("not json"));
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

    #[derive(Default)]
    struct FakeMusic {
        calls: std::sync::Mutex<Vec<String>>,
        ducks: AtomicU32,
    }

    #[async_trait::async_trait]
    impl MusicControl for FakeMusic {
        fn play(&self) -> petrovich_core::Result<()> {
            self.calls.lock().unwrap().push("play".into());
            Ok(())
        }
        async fn play_search(&self, query: &str) -> petrovich_core::Result<String> {
            self.calls.lock().unwrap().push(format!("search:{query}"));
            Ok(format!("Включаю: {query}"))
        }
        fn pause(&self) -> petrovich_core::Result<()> {
            Ok(())
        }
        fn resume(&self) -> petrovich_core::Result<()> {
            Ok(())
        }
        fn next(&self) -> petrovich_core::Result<()> {
            Ok(())
        }
        fn previous(&self) -> petrovich_core::Result<()> {
            Ok(())
        }
        fn status(&self) -> petrovich_core::Result<String> {
            Ok("готово".into())
        }
        fn duck(&self) {
            self.ducks.fetch_add(1, Ordering::Relaxed);
        }
        fn unduck(&self) {
            self.ducks.fetch_sub(1, Ordering::Relaxed);
        }
    }

    #[tokio::test]
    async fn deferred_music_reports_reason_then_delegates() {
        let slot = DeferredMusic::new("Spotify ещё подключается");
        let err = slot.play().unwrap_err().to_string();
        assert!(err.contains("подключается"), "{err}");
        assert!(slot.play_search("джаз").await.is_err());
        assert!(!slot.is_playing().unwrap());
        assert_eq!(slot.position().unwrap(), None);

        // Ассистент говорит, пока Spotify ещё не подключился: приглушение
        // должно дойти до backend после установки.
        slot.duck();
        let fake = Arc::new(FakeMusic::default());
        slot.install(fake.clone());
        assert_eq!(fake.ducks.load(Ordering::Relaxed), 1);

        slot.play().unwrap();
        assert_eq!(slot.play_search("джаз").await.unwrap(), "Включаю: джаз");
        assert_eq!(
            fake.calls.lock().unwrap().as_slice(),
            ["play", "search:джаз"]
        );

        slot.unduck();
        slot.unduck();
        assert_eq!(fake.ducks.load(Ordering::Relaxed), 0);
        assert_eq!(slot.status().unwrap(), "готово");
    }
}
