//! Жизненный цикл демона и голосовой автомат.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use skynet_audio::frontend::{AudioEngine, CaptureBlock, PlaybackHandle};
use skynet_audio::{AudioFrontend, LocalChime};
use skynet_core::config::{Config, DialogConfig, LoadedConfig};
use skynet_core::events::{UiEvent, UiNotice, VoiceState};
use skynet_core::history::ConversationHistory;
use skynet_core::ports::{Chime, ChimeSound, MusicControl, VolumeControl};
use skynet_core::profile::HardwareProfile;
use skynet_hw::ButtonEvent;
use skynet_providers::llm::OpenAiLlm;
use skynet_providers::{stt_from_config, tts_from_config};
use skynet_tools::{ControlAction, TimerManager, ToolContext, Weather, Web};
use skynet_wakeword::WakeWord;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::music::CommandMusic;
use crate::turn::{run_turn, speak_text, TurnContext, TurnOutcome};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Idle,
    Listening,
    Thinking,
}

struct TurnEvent {
    generation: u64,
    result: std::result::Result<TurnOutcome, String>,
}

struct App {
    dialog: DialogConfig,
    mode: Mode,
    muted: bool,
    follow_deadline: Option<tokio::time::Instant>,
    ignore_until: tokio::time::Instant,
    generation: u64,
    task: Option<JoinHandle<()>>,
    ui: mpsc::UnboundedSender<UiEvent>,
    playback: PlaybackHandle,
    volume: Arc<dyn VolumeControl>,
    chime: Arc<LocalChime>,
    wake: Option<WakeWord>,
    vad: skynet_audio::vad::Vad,
    frontend: AudioFrontend,
    history: ConversationHistory,
    turn: TurnContext,
    task_tx: mpsc::Sender<TurnEvent>,
    started: Instant,
    volume_step: i32,
}

pub async fn run(loaded: LoadedConfig) -> Result<()> {
    let cfg = loaded.config;
    if let Some(tz) = &cfg.daemon.timezone {
        std::env::set_var("TZ", tz);
    }
    if let Some(gid) = cfg.daemon.run_gid {
        // SAFETY: setgid до создания потоков и сокетов; UID остаётся root,
        // чтобы сохранить доступ к sysfs/GPIO.
        let rc = unsafe { libc::setgid(gid) };
        if rc != 0 {
            bail!("setgid({gid}): {}", std::io::Error::last_os_error());
        }
        tracing::info!("группа процесса: {gid}");
    }
    let profile =
        skynet_core::profile::resolve(&cfg.hardware.model).map_err(|e| anyhow::anyhow!("{e}"))?;
    tracing::info!("модель: {} ({})", profile.model_name, profile.codename);
    let wake = if cfg.wakeword.enabled {
        Some(WakeWord::load(
            &cfg.wakeword.model,
            &cfg.wakeword.assets,
            cfg.wakeword.threshold,
            cfg.wakeword.sensitivity,
            cfg.wakeword.gain,
            cfg.wakeword.refractory_ms,
        )?)
    } else {
        tracing::warn!("активация отключена в конфигурации");
        None
    };
    if let Some(w) = &wake {
        tracing::info!("активация: «{}», порог {:.3}", w.name(), w.threshold());
    }
    let audio = AudioEngine::start(&cfg.audio, profile, cfg.hardware.volume_step)?;
    let AudioEngine {
        capture,
        playback,
        volume,
    } = audio;
    let frontend = AudioFrontend::new(&cfg.audio)?;
    tracing::info!(
        "аудио: канал распознавания 16 кГц, VQE={}",
        if frontend.uses_vqe() {
            "прошивка"
        } else {
            "собственный DSP"
        }
    );

    let stt = Arc::from(stt_from_config(&cfg.stt, &loaded.secrets)?);
    let tts = Arc::from(tts_from_config(&cfg.tts, &loaded.secrets)?);
    let llm = Arc::new(OpenAiLlm::new(&cfg.llm, &loaded.secrets)?);

    let (ui_tx, ui_rx) = mpsc::unbounded_channel();
    let (announce_tx, mut announce_rx) = mpsc::channel(4);
    let timers = TimerManager::new(ui_tx.clone(), announce_tx);
    let web = Web::new(
        cfg.tools.web_timeout_ms,
        cfg.tools.web_max_bytes,
        cfg.tools.web_allow_private,
    )?;
    let music: Option<Arc<dyn MusicControl>> = CommandMusic::new(cfg.tools.music_command.clone())
        .map(|m| Arc::new(m) as Arc<dyn MusicControl>);
    let mut tools = ToolContext::new(
        volume.clone(),
        music,
        timers,
        web,
        cfg.tools.clone(),
        cfg.hardware.volume_step,
    );
    if let (Some(lat), Some(lon)) = (cfg.tools.weather_latitude, cfg.tools.weather_longitude) {
        if !cfg.tools.weather_url.is_empty() {
            tools = tools.with_weather(Weather::new(
                cfg.tools.weather_url.clone(),
                lat,
                lon,
                cfg.tools.weather_place.clone(),
                cfg.tools.web_timeout_ms,
                cfg.tools.web_allow_private,
            )?);
        }
    }
    let hardware = skynet_hw::start(profile, &cfg.hardware, ui_rx);
    let mut buttons = hardware.buttons;

    let chime = Arc::new(LocalChime::new(playback.clone()));
    let turn = TurnContext {
        llm,
        stt,
        tts,
        tools,
        playback: playback.clone(),
        ui: ui_tx.clone(),
        max_tool_rounds: cfg.dialog.max_tool_rounds,
    };
    let vad = skynet_audio::vad::Vad::new(
        cfg.audio.sample_rate,
        cfg.vad.min_speech_ms,
        cfg.vad.silence_ms,
        cfg.vad.max_utterance_ms,
        cfg.vad.energy_margin_db,
        cfg.vad.pre_roll_ms,
        cfg.vad.absolute_db,
    );
    let history = ConversationHistory::new(
        cfg.llm.system_prompt.clone(),
        cfg.dialog.history_turns,
        cfg.dialog.history_chars,
        Duration::from_secs(cfg.dialog.history_ttl_s),
    );
    let (task_tx, mut task_rx) = mpsc::channel(4);
    let mut app = App {
        dialog: cfg.dialog.clone(),
        mode: Mode::Idle,
        muted: false,
        follow_deadline: None,
        ignore_until: tokio::time::Instant::now(),
        generation: 0,
        task: None,
        ui: ui_tx,
        playback,
        volume,
        chime,
        wake,
        vad,
        frontend,
        history,
        turn,
        task_tx,
        started: Instant::now(),
        volume_step: cfg.hardware.volume_step,
    };
    app.enter_idle();

    let mut capture = capture;
    let mut sigterm = signal(SignalKind::terminate()).context("SIGTERM")?;
    let shutdown = async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    };
    tokio::pin!(shutdown);
    let pid = cfg.daemon.pid_file.clone();
    write_pid(&pid)?;
    tracing::info!("skynetd {} готов", skynet_core::VERSION);
    loop {
        let deadline = app.follow_deadline;
        let follow = async move {
            match deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = &mut shutdown => {
                tracing::info!("остановка по сигналу");
                break;
            }
            _ = follow => {
                if app.mode == Mode::Listening {
                    tracing::info!("ожидание речи истекло");
                    app.enter_idle();
                }
            }
            block = capture.recv() => {
                match block {
                    Some(b) => app.on_block(b),
                    None => break,
                }
            }
            btn = buttons.recv() => {
                if let Some(b) = btn {
                    app.on_button(b).await;
                }
            }
            fired = announce_rx.recv() => {
                if let Some(f) = fired {
                    let _ = app.chime.play(ChimeSound::Timer);
                    let ctx = app.turn.clone();
                    let text = format!("Таймер {} сработал.", f.label);
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(1100)).await;
                        if let Err(e) = speak_text(&ctx, &text).await {
                            tracing::warn!("таймер: озвучка: {e:#}");
                        }
                    });
                }
            }
            ev = task_rx.recv() => {
                if let Some(ev) = ev {
                    app.on_turn(ev);
                }
            }
        }
    }
    app.abort_turn();
    app.playback.stop();
    let _ = std::fs::remove_file(&pid);
    Ok(())
}

impl App {
    fn enter_idle(&mut self) {
        self.mode = Mode::Idle;
        self.follow_deadline = None;
        self.vad.reset_utterance();
        let _ = self.ui.send(UiEvent::State(VoiceState::Idle));
    }

    fn enter_listening(&mut self, follow_up: bool) {
        self.mode = Mode::Listening;
        self.vad.reset_utterance();
        if follow_up {
            // Не слушаем хвост собственной озвучки.
            self.ignore_until = tokio::time::Instant::now() + Duration::from_millis(400);
        }
        let ms = if follow_up {
            self.dialog.follow_up_ms
        } else {
            self.dialog.listen_timeout_ms
        };
        self.follow_deadline = Some(tokio::time::Instant::now() + Duration::from_millis(ms));
        if let Some(w) = self.wake.as_mut() {
            w.reset();
        }
        let _ = self.ui.send(UiEvent::State(VoiceState::Listening));
    }

    fn feedback(&mut self, sound: ChimeSound, notice: UiNotice) {
        let _ = self.ui.send(UiEvent::Notice(notice));
        if self.dialog.state_chimes {
            let _ = self.chime.play(sound);
        }
    }

    fn activate(&mut self) {
        if self.muted {
            return;
        }
        tracing::info!("активация: начинаю слушать");
        let _ = self.ui.send(UiEvent::Notice(UiNotice::Listening));
        if self.dialog.wake_chime {
            let _ = self.chime.play(ChimeSound::Listening);
            self.ignore_until = tokio::time::Instant::now() + Duration::from_millis(200);
        }
        self.enter_listening(false);
    }

    fn on_block(&mut self, block: CaptureBlock) {
        let frames = self.frontend.push(&block);
        let now = self.started.elapsed().as_millis() as u64;
        for frame in frames {
            if self.muted || tokio::time::Instant::now() < self.ignore_until {
                continue;
            }
            match self.mode {
                Mode::Idle => {
                    let hits = self
                        .wake
                        .as_mut()
                        .map(|w| w.push(&frame.samples, now))
                        .unwrap_or_default();
                    if let Some(d) = hits.first() {
                        tracing::info!("слово активации: score {:.4}", d.score);
                        self.activate();
                    }
                }
                Mode::Listening => {
                    let utts = self.vad.push(&frame.samples);
                    if self.vad.in_speech() {
                        self.follow_deadline = None;
                    }
                    if let Some(u) = utts.into_iter().next() {
                        tracing::info!(
                            "фраза: {} мс (речь {} мс), отправляю в STT",
                            u.speech_ms,
                            u.voiced_ms
                        );
                        self.start_turn(u.samples);
                    }
                }
                Mode::Thinking => {
                    if !self.dialog.barge_in {
                        continue;
                    }
                    // Голос поверх ответа: останавливаем озвучку сразу, не
                    // дожидаясь конца фразы. Дальше VAD в Listening завершит
                    // запись, и локальная команда «стоп» уйдёт без LLM.
                    if self.playback.is_playing() {
                        let _ = self.vad.push(&frame.samples);
                        if self.vad.in_speech() {
                            tracing::info!("бардж-ин по голосу");
                            self.abort_turn();
                            self.playback.stop();
                            self.mode = Mode::Listening;
                            self.follow_deadline = Some(
                                tokio::time::Instant::now()
                                    + Duration::from_millis(self.dialog.follow_up_ms),
                            );
                            let _ = self.ui.send(UiEvent::State(VoiceState::Listening));
                            continue;
                        }
                    }
                    let hit = self
                        .wake
                        .as_mut()
                        .is_some_and(|w| !w.push(&frame.samples, now).is_empty());
                    if hit {
                        tracing::info!("бардж-ин");
                        self.abort_turn();
                        self.playback.stop();
                        self.enter_listening(true);
                    }
                }
            }
        }
    }

    fn start_turn(&mut self, pcm: Vec<i16>) {
        self.history.expire_if_idle();
        let snapshot = self.history.snapshot();
        tracing::info!("ход диалога: STT → LLM → TTS");
        self.mode = Mode::Thinking;
        self.follow_deadline = None;
        let _ = self.ui.send(UiEvent::State(VoiceState::Thinking));
        self.feedback(ChimeSound::Heard, UiNotice::Heard);
        if let Some(w) = self.wake.as_mut() {
            w.reset();
        }
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let ctx = self.turn.clone();
        let tx = self.task_tx.clone();
        let timeout = Duration::from_millis(self.dialog.response_timeout_ms);
        self.task = Some(tokio::spawn(async move {
            let result = match tokio::time::timeout(timeout, run_turn(&ctx, snapshot, pcm)).await {
                Ok(Ok(o)) => Ok(o),
                Ok(Err(e)) => Err(format!("{e:#}")),
                Err(_) => Err("превышено время ответа".into()),
            };
            let _ = tx.send(TurnEvent { generation, result }).await;
        }));
    }

    fn on_turn(&mut self, ev: TurnEvent) {
        if ev.generation != self.generation {
            return;
        }
        self.task = None;
        match ev.result {
            Ok(outcome) => {
                tracing::info!(
                    "ответ: {}",
                    if outcome.spoken.is_empty() {
                        "<без текста>"
                    } else {
                        &outcome.spoken
                    }
                );
                for m in outcome.messages {
                    self.history.push(m);
                }
                if outcome.spoken.is_empty() && outcome.control.is_none() {
                    self.announce("Не понял. Повторите, пожалуйста.");
                    self.enter_listening(true);
                    return;
                }
                match outcome.control {
                    Some(ControlAction::ResetDialog) => {
                        self.feedback(ChimeSound::Cleared, UiNotice::Cleared);
                        self.history.reset();
                        self.enter_idle();
                    }
                    Some(ControlAction::StopSpeaking) => {
                        self.feedback(ChimeSound::Stopped, UiNotice::Stopped);
                        self.playback.stop();
                        self.enter_idle();
                    }
                    None => {
                        if !outcome.spoken.is_empty() {
                            self.feedback(ChimeSound::Done, UiNotice::Done);
                        }
                        self.enter_listening(true);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("ход диалога: {e}");
                let _ = self.ui.send(UiEvent::Error(e.clone()));
                let _ = self.ui.send(UiEvent::Notice(UiNotice::Error));
                let _ = self.chime.play(ChimeSound::Error);
                self.enter_listening(true);
            }
        }
    }

    fn announce(&self, text: &str) {
        let ctx = self.turn.clone();
        let text = text.to_owned();
        tokio::spawn(async move {
            if let Err(e) = speak_text(&ctx, &text).await {
                tracing::warn!("озвучка: {e:#}");
            }
        });
    }

    fn abort_turn(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }

    async fn on_button(&mut self, ev: ButtonEvent) {
        match ev {
            ButtonEvent::Action => match self.mode {
                Mode::Idle => self.activate(),
                Mode::Listening => {
                    self.feedback(ChimeSound::Stopped, UiNotice::Stopped);
                    self.enter_idle();
                }
                Mode::Thinking => {
                    self.abort_turn();
                    self.playback.stop();
                    self.feedback(ChimeSound::Stopped, UiNotice::Stopped);
                    self.enter_idle();
                }
            },
            ButtonEvent::VolumeUp | ButtonEvent::VolumeDown => {
                let delta = if matches!(ev, ButtonEvent::VolumeUp) {
                    self.volume_step
                } else {
                    -self.volume_step
                };
                match self.volume.step(delta) {
                    Ok(s) => {
                        let _ = self.ui.send(UiEvent::Volume(s.percent));
                    }
                    Err(e) => tracing::warn!("громкость: {e}"),
                }
            }
            ButtonEvent::Mute(m) => {
                self.muted = m;
                let _ = self.ui.send(UiEvent::Muted(m));
                self.abort_turn();
                self.playback.stop();
                if m {
                    tracing::info!("микрофоны выключены аппаратно");
                } else {
                    tracing::info!("микрофоны включены");
                }
                self.enter_idle();
            }
        }
    }
}

fn write_pid(path: &PathBuf) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    if let Ok(old) = std::fs::read_to_string(path) {
        if let Ok(pid) = old.trim().parse::<u32>() {
            if PathBuf::from(format!("/proc/{pid}")).exists() {
                bail!("уже запущен процесс {pid} ({})", path.display());
            }
        }
    }
    std::fs::write(path, std::process::id().to_string())
        .with_context(|| format!("запись {}", path.display()))?;
    Ok(())
}

/// Проверка конфигурации без запуска аудио и сети.
pub fn check(loaded: &LoadedConfig) -> Result<()> {
    let cfg: &Config = &loaded.config;
    cfg.validate().map_err(|e| anyhow::anyhow!("{e}"))?;
    let profile: &'static HardwareProfile =
        skynet_core::profile::resolve(&cfg.hardware.model).map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("модель: {} ({})", profile.model_name, profile.codename);
    if cfg.wakeword.enabled {
        check_file(&cfg.wakeword.model, "модель активации")?;
        if skynet_wakeword::micro::is_micro_manifest(&cfg.wakeword.model) {
            let manifest = std::fs::read_to_string(&cfg.wakeword.model)
                .map_err(|e| anyhow::anyhow!("{}: {e}", cfg.wakeword.model.display()))?;
            let v: serde_json::Value = serde_json::from_str(&manifest)
                .map_err(|e| anyhow::anyhow!("манифест microWakeWord: {e}"))?;
            let tflite = v
                .get("model")
                .and_then(|m| m.as_str())
                .ok_or_else(|| anyhow::anyhow!("в манифесте нет model"))?;
            let path = cfg
                .wakeword
                .model
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .join(tflite);
            check_file(&path, "TFLite-модель")?;
            println!(
                "активация: microWakeWord «{}» ({})",
                v.get("wake_word").and_then(|w| w.as_str()).unwrap_or("?"),
                cfg.wakeword.model.display()
            );
        } else {
            check_file(&cfg.wakeword.assets, "feature-backbone")?;
            skynet_wakeword::WakeModel::load(&cfg.wakeword.model)
                .map_err(|e| anyhow::anyhow!("модель активации: {e:#}"))?;
            let assets = std::fs::read(&cfg.wakeword.assets)
                .map_err(|e| anyhow::anyhow!("{}: {e}", cfg.wakeword.assets.display()))?;
            let tensors = skynet_wakeword::tensors::read(&assets)
                .map_err(|e| anyhow::anyhow!("backbone: {e:#}"))?;
            if !tensors.contains_key("mel.filters") {
                bail!("в backbone нет mel.filters");
            }
            println!("активация: {}", cfg.wakeword.model.display());
        }
    }
    println!(
        "провайдеры: STT {:?}, LLM {}, TTS {:?}",
        cfg.stt.provider, cfg.llm.model, cfg.tts.provider
    );
    for env in [
        &cfg.stt.api_key_env,
        &cfg.llm.api_key_env,
        &cfg.tts.api_key_env,
    ] {
        if loaded.secrets.get(env).is_none() {
            println!("секрет {env}: не задан (может быть не нужен локальному сервису)");
        } else {
            println!("секрет {env}: задан");
        }
    }
    if cfg.audio.vqe != skynet_core::config::VqeMode::Off {
        match skynet_audio::vqe::available() {
            Ok(()) => println!("VQE прошивки: доступен"),
            Err(e) => println!("VQE прошивки: недоступен ({e}); будет собственный DSP"),
        }
    }
    println!("инструменты: {:?}", cfg.tools.enabled);
    if cfg.tools.weather_latitude.is_some() && cfg.tools.weather_longitude.is_some() {
        println!("погода: включена");
    }
    println!("конфигурация корректна: {}", loaded.path.display());
    Ok(())
}

fn check_file(path: &std::path::Path, what: &str) -> Result<()> {
    if !path.exists() {
        bail!("{what} не найден: {}", path.display());
    }
    Ok(())
}
