//! Диагностика установки без запуска демона.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use jarvis_core::config::{Config, DEFAULT_CONFIG};
use jarvis_core::profile;

#[derive(Parser, Debug)]
#[command(version, about = "Диагностика jarvisd")]
struct Cli {
    #[arg(short, long, env = "JARVIS_CONFIG", default_value = DEFAULT_CONFIG)]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Состояние процесса и краткая сводка
    Status,
    /// Полная проверка окружения; код 1 при проблемах
    Doctor,
    /// Последние строки журнала
    Logs {
        #[arg(default_value_t = 50)]
        lines: usize,
    },
    /// Прогнать модель активации по WAV (16 кГц моно)
    WakeTest { wav: PathBuf },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let loaded = Config::load(&cli.config).map_err(|e| anyhow::anyhow!("{e}"))?;
    match cli.cmd {
        Cmd::Status => status(&loaded),
        Cmd::Doctor => doctor(&loaded),
        Cmd::Logs { lines } => logs(&loaded, lines),
        Cmd::WakeTest { wav } => wake_test(&loaded, &wav),
    }
}

fn status(loaded: &jarvis_core::LoadedConfig) -> Result<()> {
    println!("конфигурация: {}", loaded.path.display());
    let pidfile = &loaded.config.daemon.pid_file;
    match std::fs::read_to_string(pidfile)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
    {
        Some(pid) if Path::new(&format!("/proc/{pid}")).exists() => {
            println!("демон: работает, pid {pid}");
            if let Ok(s) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
                for line in s.lines().filter(|l| {
                    l.starts_with("VmRSS:") || l.starts_with("Threads:") || l.starts_with("VmPeak:")
                }) {
                    println!("  {line}");
                }
            }
            if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                if let Some(f) = stat.split_whitespace().nth(21) {
                    let ticks: u64 = f.parse().unwrap_or(0);
                    println!("  CPU: {:.1} с", ticks as f64 / 100.0);
                }
            }
        }
        _ => println!("демон: не запущен ({})", pidfile.display()),
    }
    let cfg = &loaded.config;
    println!(
        "активация: {}",
        if cfg.wakeword.enabled {
            "включена"
        } else {
            "выключена"
        }
    );
    if cfg.wakeword.enabled {
        println!("  модель: {}", cfg.wakeword.model.display());
        println!("  backbone: {}", cfg.wakeword.assets.display());
    }
    println!("LLM: {} ({})", cfg.llm.model, cfg.llm.base_url);
    println!("STT: {:?}, TTS: {:?}", cfg.stt.provider, cfg.tts.provider);
    println!("инструменты: {:?}", cfg.tools.enabled);
    Ok(())
}

fn doctor(loaded: &jarvis_core::LoadedConfig) -> Result<()> {
    let mut failures = 0;
    let ok = |m: &str| println!("  ok   {m}");
    let warn = |m: &str| println!("  WARN {m}");
    let mut fail = |m: &str| {
        println!("  FAIL {m}");
        failures += 1;
    };
    let cfg = &loaded.config;
    match profile::resolve(&cfg.hardware.model) {
        Ok(p) => ok(&format!("модель {} ({})", p.model_name, p.codename)),
        Err(e) => fail(&format!("модель: {e}")),
    }
    if cfg.wakeword.enabled {
        if cfg.wakeword.model.exists() {
            ok("модель активации найдена");
        } else {
            fail(&format!(
                "нет модели активации {}",
                cfg.wakeword.model.display()
            ));
        }
        if cfg.wakeword.assets.exists() {
            ok("feature-backbone найден");
        } else {
            fail(&format!("нет backbone {}", cfg.wakeword.assets.display()));
        }
    }
    match jarvis_audio::vqe::available() {
        Ok(()) => ok("VQE прошивки доступен"),
        Err(e) => warn(&format!(
            "VQE недоступен: {e}; используется собственный DSP"
        )),
    }
    if Command::new("amixer")
        .args(["-D", &cfg.audio.playback_pcm, "sget", "Master"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        ok("ALSA Master доступен");
    } else {
        fail("amixer не читает Master");
    }
    if let Ok(p) = profile::resolve(&cfg.hardware.model) {
        if Path::new("/proc/bus/input/devices").exists() {
            let text = std::fs::read_to_string("/proc/bus/input/devices").unwrap_or_default();
            let name = cfg
                .hardware
                .input_device
                .as_deref()
                .unwrap_or(p.input_device);
            if jarvis_hw::buttons::find_event_node(name, &text).is_some() {
                ok("кнопки найдены");
            } else {
                warn(&format!("кнопки {name} не найдены"));
            }
        }
        if p.clock_dir_path()
            .map(|d| Path::new(&d).exists())
            .unwrap_or(false)
        {
            ok("часы найдены");
        } else {
            warn("часы не найдены (возможна ревизия без панели)");
        }
        let gpio = cfg.hardware.mic_mute_gpio.or(p.mute_gpio);
        if let Some(g) = gpio {
            let path = format!("/sys/class/gpio/gpio{g}/value");
            if Path::new(&path).exists() {
                ok(&format!("GPIO мута {g} доступен"));
            } else {
                warn(&format!("GPIO мута {g} недоступен"));
            }
        }
    }
    if pids("maind").is_empty() && pids("quasar_launcher2").is_empty() {
        ok("Quasar/maind не запущены");
    } else {
        fail("Quasar или maind работают: возможны обращения к Яндексу");
    }
    let hosts = &cfg.privacy.yandex_hosts;
    let mut resolved = Vec::new();
    for h in hosts.iter().take(4) {
        if Command::new("getent")
            .args(["hosts", h])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            resolved.push(h.clone());
        }
    }
    if resolved.is_empty() {
        ok("домены Яндекса не разрешаются");
    } else {
        warn(&format!("разрешаются домены Яндекса: {resolved:?}"));
    }
    match disk_free("/data") {
        Some(kb) if kb > 20 * 1024 => ok(&format!("свободно на /data: {} МиБ", kb / 1024)),
        Some(kb) => fail(&format!("мало места на /data: {} МиБ", kb / 1024)),
        None => warn("не удалось прочитать /data"),
    }
    if let Ok(mem) = std::fs::read_to_string("/proc/meminfo") {
        if let Some(kb) = mem
            .lines()
            .find(|l| l.starts_with("MemAvailable:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
        {
            if kb < 30 * 1024 {
                warn(&format!("мало памяти: {} МиБ", kb / 1024));
            } else {
                ok(&format!("доступно памяти: {} МиБ", kb / 1024));
            }
        }
    }
    if failures == 0 {
        println!("итог: критических проблем нет");
        Ok(())
    } else {
        bail!("итог: проблем — {failures}")
    }
}

fn logs(loaded: &jarvis_core::LoadedConfig, lines: usize) -> Result<()> {
    let candidates = [
        loaded.config.daemon.state_dir.join("../log/jarvisd.log"),
        PathBuf::from("/data/jarvis/log/jarvisd.log"),
        PathBuf::from("/tmp/jarvisd.log"),
    ];
    for p in candidates {
        if let Ok(text) = std::fs::read_to_string(&p) {
            let v: Vec<&str> = text.lines().collect();
            let start = v.len().saturating_sub(lines);
            for l in &v[start..] {
                println!("{l}");
            }
            return Ok(());
        }
    }
    bail!("журнал не найден")
}

fn wake_test(loaded: &jarvis_core::LoadedConfig, path: &Path) -> Result<()> {
    let cfg = &loaded.config;
    let bytes = std::fs::read(path).with_context(|| path.display().to_string())?;
    let wav = jarvis_core::wav::parse(&bytes)?;
    let mono = wav.to_mono();
    let pcm = if wav.sample_rate != 16_000 {
        jarvis_audio::resample::resample(&mono, wav.sample_rate, 16_000)
    } else {
        mono
    };
    let mut w = jarvis_wakeword::WakeWord::load(
        &cfg.wakeword.model,
        &cfg.wakeword.assets,
        cfg.wakeword.threshold,
        cfg.wakeword.sensitivity,
        cfg.wakeword.gain,
        cfg.wakeword.refractory_ms,
    )?;
    let mut peak = 0.0f32;
    let mut hits = 0;
    for (i, chunk) in pcm.chunks(1280).enumerate() {
        for d in w.push(chunk, i as u64 * 80) {
            hits += 1;
            println!("срабатывание: {:.1} мс, score {:.4}", d.at_ms, d.score);
        }
        peak = peak.max(w.last_score());
    }
    println!(
        "модель «{}», порог {:.3}, пик {:.4}, срабатываний {hits}, длительность {} мс",
        w.name(),
        w.threshold(),
        peak,
        pcm.len() * 1000 / 16_000
    );
    Ok(())
}

fn pids(name: &str) -> Vec<u32> {
    let mut out = Vec::new();
    if let Ok(dir) = std::fs::read_dir("/proc") {
        for e in dir.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            if let Ok(cmd) = std::fs::read(format!("/proc/{pid}/comm")) {
                if String::from_utf8_lossy(&cmd).trim() == name {
                    out.push(pid);
                }
            }
        }
    }
    out
}

fn disk_free(path: &str) -> Option<u64> {
    let out = Command::new("df").args(["-k", path]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().nth(1)?.split_whitespace().nth(3)?.parse().ok()
}
