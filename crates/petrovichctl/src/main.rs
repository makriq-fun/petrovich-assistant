//! Диагностика установки без запуска демона.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use petrovich_core::config::{Config, DEFAULT_CONFIG};
use petrovich_core::profile;
use petrovich_core::text::SentenceChunker;

#[derive(Parser, Debug)]
#[command(version, about = "Диагностика petrovichd")]
struct Cli {
    #[arg(short, long, env = "PETROVICH_CONFIG", default_value = DEFAULT_CONFIG)]
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
    /// Прогнать штатный command spotter по WAV
    CommandsTest { wav: PathBuf },
    /// Записать несколько секунд с микрофонов и показать уровни каналов
    AudioTest {
        #[arg(default_value_t = 5)]
        seconds: u32,
    },
    /// Синтезировать текст текущим TTS в файл
    TtsTest {
        text: String,
        #[arg(default_value = "/tmp/petrovich-tts.pcm")]
        out: PathBuf,
    },
    /// Распознать WAV текущим STT
    SttTest { wav: PathBuf },
    /// Проверить LLM одним запросом
    LlmTest { text: String },
    /// Проверить инструмент погоды
    WeatherTest,
    /// Проверить сахар из Nightscout за N часов
    SugarTest {
        #[arg(default_value_t = 3)]
        hours: u64,
    },
    /// Проверить управление ПК; --wake дополнительно шлёт magic packet
    ComputerTest {
        #[arg(long)]
        wake: bool,
    },
    /// Проверить веб-поиск
    SearchTest { query: String },
    /// Проверить прямой HTTP(S)-доступ без прокси
    FetchTest {
        url: String,
        #[arg(long)]
        proxy: Option<String>,
    },
    /// Прочитать датчик освещённости (5 отсчётов)
    LightTest,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let loaded = Config::load(&cli.config).map_err(|e| anyhow::anyhow!("{e}"))?;
    if matches!(
        &cli.cmd,
        Cmd::LlmTest { .. }
            | Cmd::SttTest { .. }
            | Cmd::TtsTest { .. }
            | Cmd::WeatherTest
            | Cmd::SugarTest { .. }
            | Cmd::SearchTest { .. }
            | Cmd::FetchTest { .. }
    ) {
        enter_network_gid(&loaded.config);
    }
    match cli.cmd {
        Cmd::Status => status(&loaded),
        Cmd::Doctor => doctor(&loaded),
        Cmd::Logs { lines } => logs(&loaded, lines),
        Cmd::WakeTest { wav } => wake_test(&loaded, &wav),
        Cmd::CommandsTest { wav } => commands_test(&loaded, &wav),
        Cmd::AudioTest { seconds } => audio_test(&loaded, seconds),
        Cmd::TtsTest { text, out } => tts_test(&loaded, &text, &out),
        Cmd::SttTest { wav } => stt_test(&loaded, &wav),
        Cmd::LlmTest { text } => llm_test(&loaded, &text),
        Cmd::WeatherTest => weather_test(&loaded),
        Cmd::SugarTest { hours } => sugar_test(&loaded, hours),
        Cmd::ComputerTest { wake } => computer_test(&loaded, wake),
        Cmd::SearchTest { query } => search_test(&loaded, &query),
        Cmd::FetchTest { url, proxy } => fetch_test(&loaded, &url, proxy.as_deref()),
        Cmd::LightTest => light_test(&loaded),
    }
}

/// Демон работает с `run_gid` (например, 2200), чтобы его трафик попал в
/// прозрачный прокси. Диагностика должна ходить так же, иначе прямой запрос
/// с root-gid получает 403 от шлюза, хотя сам помощник работает.
fn enter_network_gid(cfg: &petrovich_core::config::Config) {
    let Some(gid) = cfg.daemon.run_gid else {
        return;
    };
    // SAFETY: setgid вызывается до сетевых запросов; UID остаётся root.
    if unsafe { libc::getegid() } == gid || unsafe { libc::setgid(gid) } == 0 {
        return;
    }
    eprintln!(
        "предупреждение: setgid({gid}) не удался: {}",
        std::io::Error::last_os_error()
    );
}

fn status(loaded: &petrovich_core::LoadedConfig) -> Result<()> {
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
                let f: Vec<&str> = stat.split_whitespace().collect();
                if f.len() > 14 {
                    let ticks: u64 =
                        f[13].parse::<u64>().unwrap_or(0) + f[14].parse::<u64>().unwrap_or(0);
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

fn doctor(loaded: &petrovich_core::LoadedConfig) -> Result<()> {
    let mut failures = 0;
    let ok = |m: &str| println!("  ok   {m}");
    let warn = |m: &str| println!("  WARN {m}");
    let mut fail = |m: &str| {
        println!("  FAIL {m}");
        failures += 1;
    };
    let cfg = &loaded.config;
    if cfg.vad.short_silence_ms < 1000 || cfg.vad.resume_pause_ms < 700 {
        warn(&format!(
            "VAD обрывает паузы: short_silence={} мс, resume={} мс (ожидается >=1000/700)",
            cfg.vad.short_silence_ms, cfg.vad.resume_pause_ms
        ));
    } else {
        ok("VAD пережидает естественные паузы");
    }
    if cfg.audio.mic_gain > 0 {
        let out = Command::new("amixer")
            .args(["-c", "0", "sget", "PDM Gain"])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let text = String::from_utf8_lossy(&o.stdout);
                if text.contains("Playback channels") && text.contains("Mono:") {
                    let level = text
                        .split("Mono:")
                        .nth(1)
                        .and_then(|v| v.split_whitespace().next())
                        .unwrap_or("?");
                    if level.starts_with('0') {
                        fail("PDM Gain = 0: микрофон не слышит");
                    } else {
                        ok(&format!("PDM Gain {level}"));
                    }
                } else {
                    warn("PDM Gain не найден");
                }
            }
            _ => warn("amixer не читает PDM Gain"),
        }
    }
    for (name, proxy) in [
        ("STT", cfg.proxy_for(&cfg.stt.proxy)),
        ("LLM", cfg.proxy_for(&cfg.llm.proxy)),
        ("TTS", cfg.proxy_for(&cfg.tts.proxy)),
        ("Tavily", cfg.proxy_for(&cfg.tools.search_proxy)),
        ("Spotify", cfg.proxy_for(&cfg.spotify.proxy)),
    ] {
        let p = proxy.trim();
        if p.is_empty() {
            continue;
        }
        let Some(rest) = p
            .strip_prefix("socks5h://")
            .or_else(|| p.strip_prefix("socks5://"))
            .or_else(|| p.strip_prefix("http://"))
            .or_else(|| p.strip_prefix("https://"))
        else {
            fail(&format!("{name}: неверный proxy {p}"));
            continue;
        };
        let addr = rest.split('/').next().unwrap_or(rest);
        match addr.to_socket_addrs().ok().and_then(|mut a| a.next()) {
            Some(addr) => {
                let timeout = std::time::Duration::from_millis(700);
                if std::net::TcpStream::connect_timeout(&addr, timeout).is_ok() {
                    ok(&format!("{name}: прокси {addr} доступен"));
                } else {
                    fail(&format!("{name}: прокси {addr} не отвечает"));
                }
            }
            None => warn(&format!("{name}: не разобрать proxy {p}")),
        }
    }
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
        if petrovich_wakeword::micro::is_micro_manifest(&cfg.wakeword.model) {
            ok("движок microWakeWord");
        } else if cfg.wakeword.assets.exists() {
            ok("feature-backbone найден");
        } else {
            fail(&format!("нет backbone {}", cfg.wakeword.assets.display()));
        }
    }
    match petrovich_audio::vqe::available() {
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
            if petrovich_hw::buttons::find_event_node(name, &text).is_some() {
                ok("кнопки найдены");
            } else {
                warn(&format!("кнопки {name} не найдены"));
            }
        }
        if Path::new(&p.clock_path("A1")).exists() {
            ok("часы найдены");
        } else {
            warn("часы не найдены (возможна ревизия без панели)");
        }
        match petrovich_hw::light::LightSensor::open(cfg.hardware.light_sensor.as_deref()) {
            Some(mut s) => match s.read() {
                Some(lux) => ok(&format!(
                    "датчик освещённости: {lux:.1} лк ({})",
                    s.path().display()
                )),
                None => warn("датчик освещённости найден, но значение не читается"),
            },
            None => warn("датчик освещённости не найден; яркость фиксированная"),
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
    let ips = resolve_hosts(&cfg.privacy.yandex_hosts);
    if ips.is_empty() {
        ok("домены Яндекса не разрешаются");
    } else {
        warn(&format!(
            "домены Яндекса разрешаются в {} адрес(ов)",
            ips.len()
        ));
        match active_remote_ips() {
            Ok(conns) => {
                let hit: Vec<String> = conns
                    .iter()
                    .filter(|ip| ips.contains(ip))
                    .map(ToString::to_string)
                    .collect();
                if hit.is_empty() {
                    ok("активных TCP-соединений с найденными адресами нет");
                } else {
                    fail(&format!("есть TCP-соединения с Яндексом: {hit:?}"));
                }
            }
            Err(e) => warn(&format!("не удалось прочитать /proc/net/tcp: {e}")),
        }
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

fn logs(loaded: &petrovich_core::LoadedConfig, lines: usize) -> Result<()> {
    let candidates = [
        loaded.config.daemon.state_dir.join("../log/petrovichd.log"),
        PathBuf::from("/data/petrovich/log/petrovichd.log"),
        PathBuf::from("/tmp/petrovichd.log"),
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

fn wake_test(loaded: &petrovich_core::LoadedConfig, path: &Path) -> Result<()> {
    let cfg = &loaded.config;
    let bytes = std::fs::read(path).with_context(|| path.display().to_string())?;
    let wav = petrovich_core::wav::parse(&bytes)?;
    let mono = wav.to_mono();
    let pcm = if wav.sample_rate != 16_000 {
        petrovich_audio::resample::resample(&mono, wav.sample_rate, 16_000)
    } else {
        mono
    };
    let mut w = petrovich_wakeword::WakeWord::load(
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

fn commands_test(loaded: &petrovich_core::LoadedConfig, path: &Path) -> Result<()> {
    let cfg = &loaded.config;
    if !cfg.commands.enabled {
        bail!("[commands] выключен в конфигурации");
    }
    let mut spotter = petrovich_audio::spotter::CommandSpotter::open(&cfg.commands.model)
        .map_err(|e| anyhow::anyhow!("command spotter: {e:#}"))?;
    let bytes = std::fs::read(path).with_context(|| path.display().to_string())?;
    let wav = petrovich_core::wav::parse(&bytes)?;
    let mono = wav.to_mono();
    let pcm = if wav.sample_rate != 16_000 {
        petrovich_audio::resample::resample(&mono, wav.sample_rate, 16_000)
    } else {
        mono
    };
    let mut found = 0;
    for chunk in pcm.chunks(320) {
        for command in spotter.feed(chunk) {
            found += 1;
            println!("команда: {command:?}");
        }
    }
    println!(
        "command spotter: {}, команд {found}, длительность {} мс",
        cfg.commands.model.display(),
        pcm.len() * 1000 / 16_000
    );
    Ok(())
}

fn resolve_hosts(hosts: &[String]) -> Vec<IpAddr> {
    let mut out = Vec::new();
    for h in hosts.iter().take(8) {
        if let Ok(addrs) = (h.as_str(), 443).to_socket_addrs() {
            for a in addrs {
                if !out.contains(&a.ip()) {
                    out.push(a.ip());
                }
            }
        }
    }
    out
}

fn active_remote_ips() -> Result<Vec<IpAddr>> {
    let mut out = Vec::new();
    for (path, v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 4 || f[3] != "01" {
                continue;
            }
            let Some((_, hex)) = f[2].split_once(':') else {
                continue;
            };
            if let Some(ip) = parse_proc_ip(hex, v6) {
                if !out.contains(&ip) {
                    out.push(ip);
                }
            }
        }
    }
    Ok(out)
}

fn parse_proc_ip(hex: &str, v6: bool) -> Option<IpAddr> {
    if v6 {
        if hex.len() != 32 {
            return None;
        }
        let mut bytes = [0u8; 16];
        for word in 0..4 {
            let v = u32::from_str_radix(&hex[word * 8..word * 8 + 8], 16).ok()?;
            bytes[word * 4..word * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        Some(IpAddr::V6(Ipv6Addr::from(bytes)))
    } else {
        if hex.len() != 8 {
            return None;
        }
        let v = u32::from_str_radix(hex, 16).ok()?;
        Some(IpAddr::V4(Ipv4Addr::from(v.swap_bytes())))
    }
}

fn tts_test(loaded: &petrovich_core::LoadedConfig, text: &str, out: &Path) -> Result<()> {
    use futures_util::StreamExt as _;
    let rt = tokio::runtime::Runtime::new()?;
    let mut tts_cfg = loaded.config.tts.clone();
    tts_cfg.proxy = loaded.config.proxy_for(&tts_cfg.proxy).to_owned();
    let tts = petrovich_providers::tts_from_config(&tts_cfg, &loaded.secrets)?;
    let mut audio = rt.block_on(tts.synthesize(text))?;
    let mut bytes = Vec::new();
    rt.block_on(async {
        while let Some(chunk) = audio.stream.next().await {
            bytes.extend_from_slice(&chunk?);
        }
        Ok::<(), anyhow::Error>(())
    })?;
    std::fs::write(out, &bytes)?;
    let samples = bytes.len() / 2;
    println!(
        "TTS: {} байт, {:.2} с при {} Гц; файл {}",
        bytes.len(),
        samples as f64 / f64::from(audio.sample_rate),
        audio.sample_rate,
        out.display()
    );
    Ok(())
}

fn stt_test(loaded: &petrovich_core::LoadedConfig, wav: &Path) -> Result<()> {
    let bytes = std::fs::read(wav)?;
    petrovich_core::wav::parse(&bytes)?;
    let mut stt_cfg = loaded.config.stt.clone();
    stt_cfg.proxy = loaded.config.proxy_for(&stt_cfg.proxy).to_owned();
    let stt = petrovich_providers::stt_from_config(&stt_cfg, &loaded.secrets)?;
    let rt = tokio::runtime::Runtime::new()?;
    let text = rt.block_on(stt.transcribe(bytes))?;
    println!("STT: {text}");
    Ok(())
}

fn light_test(loaded: &petrovich_core::LoadedConfig) -> Result<()> {
    let mut sensor =
        petrovich_hw::light::LightSensor::open(loaded.config.hardware.light_sensor.as_deref())
            .ok_or_else(|| anyhow::anyhow!("датчик освещённости не найден"))?;
    println!("датчик: {}", sensor.path().display());
    for i in 0..5 {
        match sensor.read() {
            Some(lux) => println!("{i}: {lux:.1} лк"),
            None => println!("{i}: значение не читается"),
        }
        if i < 4 {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
    Ok(())
}

fn weather_test(loaded: &petrovich_core::LoadedConfig) -> Result<()> {
    let t = &loaded.config.tools;
    let lat = t
        .weather_latitude
        .ok_or_else(|| anyhow::anyhow!("нет weather_latitude"))?;
    let lon = t
        .weather_longitude
        .ok_or_else(|| anyhow::anyhow!("нет weather_longitude"))?;
    let w = petrovich_tools::Weather::new(
        t.weather_url.clone(),
        lat,
        lon,
        t.weather_place.clone(),
        t.web_timeout_ms,
        t.web_allow_private,
    )?
    .with_proxy(loaded.config.network.proxy.trim());
    let rt = tokio::runtime::Runtime::new()?;
    println!("{}", rt.block_on(w.current())?);
    Ok(())
}

fn sugar_test(loaded: &petrovich_core::LoadedConfig, hours: u64) -> Result<()> {
    let t = &loaded.config.tools;
    if t.nightscout_url.is_empty() {
        bail!("Nightscout не настроен: tools.nightscout_url пуст");
    }
    let token = loaded
        .secrets
        .get(&t.nightscout_token_env)
        .unwrap_or_default();
    let ns = petrovich_tools::Nightscout::new(
        t.nightscout_url.clone(),
        token,
        t.web_timeout_ms,
        t.web_allow_private,
    )?
    .with_proxy(loaded.config.network.proxy.trim());
    let rt = tokio::runtime::Runtime::new()?;
    println!("{}", rt.block_on(ns.sugar(Some(hours)))?);
    Ok(())
}

fn computer_test(loaded: &petrovich_core::LoadedConfig, wake: bool) -> Result<()> {
    let cfg = loaded
        .config
        .tools
        .computer
        .clone()
        .ok_or_else(|| anyhow::anyhow!("[tools.computer] не задан"))?;
    let pc = petrovich_tools::Computer::new(cfg)?;
    let rt = tokio::runtime::Runtime::new()?;
    if wake {
        rt.block_on(pc.wake())?;
        println!("magic packet отправлен на {}", pc.config().mac);
    }
    let answer = rt.block_on(pc.probe())?;
    println!("ПК на связи: {}", answer.trim());
    Ok(())
}

fn fetch_test(loaded: &petrovich_core::LoadedConfig, url: &str, proxy: Option<&str>) -> Result<()> {
    let t = &loaded.config.tools;
    let web = petrovich_tools::Web::with_fetch_proxy(
        t.web_timeout_ms,
        t.web_max_bytes,
        false,
        proxy.unwrap_or_else(|| loaded.config.network.proxy.trim()),
    )?;
    let rt = tokio::runtime::Runtime::new()?;
    let page = rt.block_on(web.fetch(url))?;
    println!("HTTP OK: {} байт, {}", page.text.len(), page.url);
    Ok(())
}

fn search_test(loaded: &petrovich_core::LoadedConfig, query: &str) -> Result<()> {
    let t = &loaded.config.tools;
    let search_key = loaded
        .secrets
        .get(&t.search_api_key_env)
        .map(str::to_owned)
        .unwrap_or_default();
    if search_key.is_empty() {
        anyhow::bail!("поиск не настроен: нужен ключ Tavily в secrets.env");
    }
    let web = petrovich_tools::Web::with_search_proxy(
        t.web_timeout_ms,
        t.web_max_bytes,
        t.web_allow_private,
        loaded.config.proxy_for(&t.search_proxy),
    )?
    .with_fetch_client_proxy(t.web_timeout_ms, loaded.config.network.proxy.trim())?
    .with_search_api(search_key);
    let rt = tokio::runtime::Runtime::new()?;
    let hits = rt.block_on(web.search(query))?;
    if hits.is_empty() {
        println!("Ничего не найдено.");
        return Ok(());
    }
    for (i, hit) in hits.iter().enumerate() {
        println!("{}. {}\n{}\n{}", i + 1, hit.title, hit.url, hit.snippet);
    }
    Ok(())
}

fn llm_test(loaded: &petrovich_core::LoadedConfig, text: &str) -> Result<()> {
    let mut llm_cfg = loaded.config.llm.clone();
    llm_cfg.proxy = loaded.config.proxy_for(&llm_cfg.proxy).to_owned();
    let llm = petrovich_providers::OpenAiLlm::new(&llm_cfg, &loaded.secrets)?;
    let rt = tokio::runtime::Runtime::new()?;
    let mut messages = Vec::new();
    if !loaded.config.llm.system_prompt.trim().is_empty() {
        messages.push(petrovich_core::chat::ChatMessage::system(
            &loaded.config.llm.system_prompt,
        ));
    }
    messages.push(petrovich_core::chat::ChatMessage::user(text));
    let mut rx = rt.block_on(llm.chat_stream(messages, Vec::new()))?;
    let mut out = String::new();
    rt.block_on(async {
        while let Some(ev) = rx.recv().await {
            match ev {
                petrovich_core::chat::LlmEvent::TextDelta(t) => out.push_str(&t),
                petrovich_core::chat::LlmEvent::Error(e) => anyhow::bail!(e),
                _ => {}
            }
        }
        Ok::<(), anyhow::Error>(())
    })?;
    let mut chunker = SentenceChunker::default();
    let mut sentences = Vec::new();
    chunker.push(&out, &mut sentences);
    if let Some(s) = chunker.flush() {
        sentences.push(s);
    }
    let mut spoken = sentences.join(" ");
    if chunker.code_seen() && !spoken.contains("Код не пишу.") {
        spoken.push_str(" Код не пишу.");
    }
    println!("LLM: {}", spoken.trim());
    if chunker.code_seen() {
        eprintln!("код вырезан из озвучки");
    }
    Ok(())
}

fn audio_test(loaded: &petrovich_core::LoadedConfig, seconds: u32) -> Result<()> {
    let cfg = &loaded.config;
    let path = std::env::temp_dir().join(format!("petrovich-audio-{}.wav", std::process::id()));
    let out = Command::new("arecord")
        .args([
            "-D",
            &cfg.audio.capture_pcm,
            "-f",
            "S16_LE",
            "-r",
            &cfg.audio.capture_rate.to_string(),
            "-c",
            &cfg.audio.capture_channels.to_string(),
            "-d",
            &seconds.to_string(),
            path.to_str().unwrap_or("/tmp/petrovich-audio.wav"),
        ])
        .output()
        .context("arecord не запущен")?;
    if !out.status.success() {
        bail!("arecord: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let wav = petrovich_core::wav::parse(&std::fs::read(&path)?)?;
    let _ = std::fs::remove_file(&path);
    println!(
        "запись: {} Гц, {} каналов, {} мс",
        wav.sample_rate,
        wav.channels,
        wav.duration_ms()
    );
    for ch in 0..wav.channels as usize {
        let vals: Vec<i16> = wav
            .samples
            .iter()
            .skip(ch)
            .step_by(wav.channels as usize)
            .copied()
            .collect();
        let rms = (vals.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>()
            / vals.len().max(1) as f64)
            .sqrt();
        let db = if rms <= 1.0 {
            -120.0
        } else {
            20.0 * (rms / 32768.0).log10()
        };
        let clip = vals.iter().filter(|v| v.unsigned_abs() >= 32700).count();
        println!("  канал {ch}: RMS {db:.1} dBFS, клипов {clip}");
    }
    println!(
        "VQE: {}",
        if petrovich_audio::vqe::available().is_ok() {
            "доступен"
        } else {
            "нет"
        }
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
            if let Ok(cmd) = std::fs::read(format!("/proc/{pid}/cmdline")) {
                let text = String::from_utf8_lossy(&cmd);
                if text
                    .split('\0')
                    .any(|arg| arg.rsplit('/').next() == Some(name))
                {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_ipv4_and_ipv6() {
        assert_eq!(
            parse_proc_ip("0100007F", false),
            Some(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)))
        );
        assert_eq!(parse_proc_ip("0100007F", true), None);
        let v6 = parse_proc_ip("00000000000000000000000001000000", true);
        assert_eq!(v6, Some(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }
}
