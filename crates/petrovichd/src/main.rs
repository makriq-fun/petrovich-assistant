//! petrovichd — независимый голосовой помощник для Яндекс Станций.

mod daemon;
mod music;
mod tts_cache;
mod turn;

use std::path::PathBuf;

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
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let loaded = Config::load(&args.config).map_err(|e| anyhow::anyhow!("{e}"))?;
    if args.check_config {
        return daemon::check(&loaded);
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
