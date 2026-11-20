//! petrovichd — независимый голосовой помощник для Яндекс Станций.

mod byedpi;
mod daemon;
mod music;
mod netbridge;
mod spotify;
mod tts_cache;
mod turn;
mod youtube;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use petrovich_core::config::{Config, DEFAULT_CONFIG};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(version, about = "Независимый голосовой помощник для Яндекс Станций")]
struct Args {
    /// Путь к config.toml
    #[arg(short, long, env = "PETROVICH_CONFIG", default_value = DEFAULT_CONFIG)]
    config: PathBuf,
    /// Проверить конфигурацию и файлы, не запуская аудио
    #[arg(long)]
    check_config: bool,
    /// Проверить YouTube без звука: найти, получить HLS и декодировать сегмент
    #[arg(long, value_name = "ЗАПРОС")]
    youtube_test: Option<String>,
    /// Проверить HTTPS-запрос через клиент и прокси YouTube
    #[arg(long, value_name = "URL")]
    http_test: Option<String>,
    /// Проиграть 10 секунд звука YouTube (диагностика ALSA)
    #[arg(long, value_name = "ЗАПРОС")]
    youtube_play: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let loaded = Config::load(&args.config).map_err(|e| anyhow::anyhow!("{e}"))?;
    if args.check_config {
        return daemon::check(&loaded);
    }
    if let Some(q) = args.youtube_test {
        println!("{}", youtube::check(&loaded.config.tools, &q).await?);
        return Ok(());
    }
    if let Some(u) = args.http_test {
        println!("{}", youtube::http_check(&loaded.config.tools, &u).await?);
        return Ok(());
    }
    if let Some(q) = args.youtube_play {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
            )
            .with_writer(std::io::stderr)
            .init();
        let p = youtube::YouTubePlayer::new(
            &loaded.config.tools,
            loaded.config.audio.playback_pcm.clone(),
        )?;
        println!("{}", p.play_impl(&q).await?);
        tokio::time::sleep(Duration::from_secs(10)).await;
        p.stop_playback();
        return Ok(());
    }
    let filter = std::env::var("RUST_LOG")
        .ok()
        .and_then(|s| EnvFilter::try_new(s).ok())
        .unwrap_or_else(|| {
            EnvFilter::try_new(&loaded.config.daemon.log_level)
                .unwrap_or_else(|_| EnvFilter::new("info"))
        });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    daemon::run(loaded).await
}
