//! Жизненный цикл демона и голосовой автомат.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use petrovich_audio::frontend::{AudioEngine, CaptureBlock, PlaybackHandle};
use petrovich_audio::spotter::{Command, CommandSpotter};
use petrovich_audio::{AudioFrontend, LocalChime};
use petrovich_core::config::{Config, DialogConfig, LoadedConfig};
use petrovich_core::events::{UiEvent, UiNotice, VoiceState};
use petrovich_core::history::ConversationHistory;
use petrovich_core::memory::{inject_shared, Memory, SharedMemory};
use petrovich_core::ports::{Chime, ChimeSound, MusicControl, VolumeControl};
use petrovich_core::profile::HardwareProfile;
use petrovich_hw::ButtonEvent;
use petrovich_providers::llm::OpenAiLlm;
use petrovich_providers::{stt_from_config, tts_from_config};
use petrovich_tools::{
    AlarmManager, ControlAction, Nightscout, TimerManager, ToolContext, Weather, Web,
};
use petrovich_wakeword::WakeWord;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::byedpi::ByeDpi;
use crate::music::CommandMusic;
use crate::spotify::Spotify;
use crate::turn::{run_turn, speak_text, TurnContext, TurnOutcome};
use crate::youtube::YouTubePlayer;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Idle,
    Listening,
    Thinking,
}

/// Что именно останавливает глобальная «пауза». Только одно, самое
/// «переднее»: активный ход ассистента (`Thinking`) важнее музыки/видео;
/// звучащая музыка или видео важнее пассивного `Listening`; если медиа нет,
/// останавливается и пассивное ожидание.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopTarget {
    Assistant,
    Media,
    None,
}

fn stop_target(mode: Mode, media_playing: bool) -> StopTarget {
    match mode {
        Mode::Thinking => StopTarget::Assistant,
        Mode::Listening if media_playing => StopTarget::Media,
        Mode::Listening => StopTarget::Assistant,
        Mode::Idle if media_playing => StopTarget::Media,
        Mode::Idle => StopTarget::None,
    }
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
    /// Жёсткий предел текущего прослушивания: не даёт VAD и шуму
    /// продлевать сессию бесконечно.
    hard_deadline: Option<tokio::time::Instant>,
    ignore_until: tokio::time::Instant,
    command_ignore_until: tokio::time::Instant,
    max_utterance_ms: u64,
    generation: u64,
    task: Option<JoinHandle<()>>,
    ui: mpsc::UnboundedSender<UiEvent>,
    playback: PlaybackHandle,
    youtube: Option<Arc<YouTubePlayer>>,
    alarms: AlarmManager,
    /// Звук до активации: «Петрович, какая погода?» без паузы после имени.
    pre_roll: VecDeque<Vec<i16>>,
    pre_roll_samples: usize,
    volume: Arc<dyn VolumeControl>,
    music: Option<Arc<dyn MusicControl>>,
    chime: Arc<LocalChime>,
    commands: Option<CommandSpotter>,
    wake: Option<WakeWord>,
    vad: petrovich_audio::vad::Vad,
    frontend: AudioFrontend,
    history: ConversationHistory,
    memory: Option<SharedMemory>,
    turn: TurnContext,
    task_tx: mpsc::Sender<TurnEvent>,
    started: Instant,
    volume_step: i32,
    /// Будильник, который сейчас звонит: «стоп» его выключает.
    ringing_alarm: Option<u64>,
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
    let profile = petrovich_core::profile::resolve(&cfg.hardware.model)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
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
    if cfg.audio.mic_gain > 0 {
        if let Err(e) = petrovich_audio::volume::set_mic_gain(0, cfg.audio.mic_gain) {
            tracing::warn!("усиление микрофона: {e:#}");
        } else {
            tracing::info!("микрофон: PDM Gain {}%", cfg.audio.mic_gain);
        }
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
    let (alarm_tx, mut alarm_rx) = mpsc::channel(4);
    let timers = TimerManager::new(ui_tx.clone(), announce_tx).with_state(cfg.timer_state_path());
    let alarms = AlarmManager::with_config(
        cfg.alarms.clone(),
        Some(cfg.alarm_state_path()),
        ui_tx.clone(),
        alarm_tx,
    );
    timers.restore().await;
    alarms.restore().await;
    let chime = Arc::new(LocalChime::new(playback.clone()));
    let search_key = if cfg.modules.web_search {
        loaded
            .secrets
            .get(&cfg.tools.search_api_key_env)
            .map(str::to_owned)
            .unwrap_or_default()
    } else {
        tracing::info!("веб-поиск выключен в конфигурации");
        String::new()
    };
    let web = Web::with_search_proxy(
        cfg.tools.web_timeout_ms,
        cfg.tools.web_max_bytes,
        cfg.tools.web_allow_private,
        &cfg.tools.search_proxy,
    )?
    .with_search_api(search_key);
    let spotify = if cfg.modules.spotify {
        let mut scfg = cfg.spotify.clone();
        if scfg.cache_dir.is_none() {
            scfg.cache_dir = Some(cfg.spotify_cache_dir());
        }
        match Spotify::start(
            &scfg,
            &loaded.secrets,
            cfg.audio.playback_pcm.clone(),
            ui_tx.clone(),
            Some(cfg.daemon.state_dir.join("spotify.json")),
        )
        .await
        {
            Ok(Some(s)) => {
                tracing::info!("Spotify Connect: «{}»", scfg.device_name);
                Some(s)
            }
            Ok(None) => {
                tracing::warn!("Spotify включён, но нет учётных данных; модуль не запущен");
                None
            }
            Err(e) => {
                tracing::warn!("Spotify недоступен: {e:#}");
                None
            }
        }
    } else {
        tracing::info!("Spotify выключен в конфигурации");
        None
    };
    let music: Option<Arc<dyn MusicControl>> = match &spotify {
        Some(s) => Some(s.clone() as Arc<dyn MusicControl>),
        None => CommandMusic::new(cfg.tools.music_command.clone())
            .map(|m| Arc::new(m) as Arc<dyn MusicControl>),
    };
    let app_music = music.clone();
    let commands = if cfg.commands.enabled {
        match CommandSpotter::open(&cfg.commands.model) {
            Ok(spotter) => {
                tracing::info!("быстрые команды: {}", cfg.commands.model.display());
                Some(spotter)
            }
            Err(e) => {
                tracing::warn!(
                    "быстрые команды недоступны ({}): {e:#}",
                    cfg.commands.model.display()
                );
                None
            }
        }
    } else {
        None
    };
    let youtube_on = cfg.modules.youtube && cfg.tools.enabled.iter().any(|t| t == "youtube");
    let mut tools_cfg = cfg.tools.clone();
    let byedpi = if youtube_on && cfg.modules.byedpi && cfg.tools.youtube_proxy.trim().is_empty() {
        match ByeDpi::start(&cfg.byedpi, cfg.byedpi_state_path()).await {
            Ok(b) => Some(b),
            Err(e) => {
                tracing::warn!("ByeDPI недоступен: {e:#}");
                None
            }
        }
    } else {
        if cfg.modules.byedpi && !cfg.tools.youtube_proxy.trim().is_empty() {
            tracing::info!("YouTube: внешний прокси важнее встроенного ByeDPI");
        }
        None
    };
    if let Some(b) = &byedpi {
        tools_cfg.youtube_proxy = b.proxy_url();
    }
    let youtube = if youtube_on {
        Some(
            YouTubePlayer::new(&tools_cfg, cfg.audio.playback_pcm.clone())?
                .with_state_path(cfg.youtube_state_path()),
        )
    } else {
        tracing::info!("YouTube выключен в конфигурации");
        None
    };
    if let (Some(bp), Some(yt)) = (byedpi.clone(), youtube.clone()) {
        let tools = tools_cfg.clone();
        let video = bp.probe_video().to_owned();
        let bp_probe = bp.clone();
        let busy = Arc::new(move || yt.is_playing());
        tokio::spawn(async move {
            let probe = {
                let tools = tools.clone();
                let video = video.clone();
                move || {
                    let tools = tools.clone();
                    let video = video.clone();
                    async move { crate::youtube::check(&tools, &video).await }
                }
            };
            match bp_probe.probe(probe.clone()).await {
                Ok(label) => tracing::info!("ByeDPI: выбрана стратегия «{label}»"),
                Err(e) => tracing::warn!("ByeDPI: проверка не удалась: {e:#}"),
            }
            let video = bp_probe.probe_video().to_owned();
            let tools = tools.clone();
            let probe = move || {
                let tools = tools.clone();
                let video = video.clone();
                async move { crate::youtube::check(&tools, &video).await }
            };
            bp_probe.refresh_loop(probe, busy).await;
        });
    }
    let memory = if cfg.memory.enabled {
        let path = cfg.memory_path();
        let memory = Memory::load(
            &path,
            cfg.memory.max_entries,
            cfg.memory.max_chars,
            cfg.memory.prompt_chars,
        );
        tracing::info!("память: {} фактов ({})", memory.len(), path.display());
        Some(Arc::new(RwLock::new(memory)))
    } else {
        None
    };
    let app_alarms = alarms.clone();
    let mut tools = ToolContext::new(
        volume.clone(),
        music,
        timers,
        alarms,
        web,
        chime.clone(),
        ui_tx.clone(),
        tools_cfg.clone(),
        cfg.hardware.volume_step,
    );
    if let Some(yt) = &youtube {
        tools = tools.with_youtube(yt.clone());
    }
    if cfg.modules.weather && !cfg.tools.weather_url.is_empty() {
        let weather = match (cfg.tools.weather_latitude, cfg.tools.weather_longitude) {
            (Some(lat), Some(lon)) => Weather::new(
                cfg.tools.weather_url.clone(),
                lat,
                lon,
                cfg.tools.weather_place.clone(),
                cfg.tools.web_timeout_ms,
                cfg.tools.web_allow_private,
            )?,
            _ => Weather::without_coordinates(
                cfg.tools.weather_url.clone(),
                cfg.tools.web_timeout_ms,
                cfg.tools.web_allow_private,
            )?,
        };
        tools = tools.with_weather(weather.with_geocoding(cfg.tools.weather_geocoding_url.clone()));
    }
    if cfg.modules.nightscout && !cfg.tools.nightscout_url.is_empty() {
        let token = loaded
            .secrets
            .get(&cfg.tools.nightscout_token_env)
            .unwrap_or_default();
        if !cfg.tools.nightscout_token_env.is_empty() && token.is_empty() {
            tracing::warn!(
                "Nightscout: {} не задан в secrets.env; запросы без токена",
                cfg.tools.nightscout_token_env
            );
        }
        tools = tools.with_nightscout(Nightscout::new(
            cfg.tools.nightscout_url.clone(),
            token,
            cfg.tools.web_timeout_ms,
            cfg.tools.web_allow_private,
        )?);
    }
    if let Some(memory) = &memory {
        tools = tools.with_memory(memory.clone());
    }
    let mut hw_cfg = cfg.hardware.clone();
    let mut indication = cfg.indication.clone();
    if !cfg.modules.display {
        hw_cfg.leds = false;
        hw_cfg.clock = false;
        hw_cfg.auto_brightness = false;
        indication.weather = false;
        indication.glucose = false;
        indication.playback = false;
        indication.timer = false;
        tracing::info!("экран и светодиоды выключены в конфигурации");
    }
    let hardware = petrovich_hw::start(profile, &hw_cfg, &indication, ui_rx);
    let mut buttons = hardware.buttons;

    let turn = TurnContext {
        llm,
        stt,
        tts,
        tts_cache: crate::tts_cache::TtsCache::default(),
        tools,
        playback: playback.clone(),
        ui: ui_tx.clone(),
        max_tool_rounds: cfg.dialog.max_tool_rounds,
        cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let vad = petrovich_audio::vad::Vad::new(
        cfg.audio.sample_rate,
        cfg.vad.min_speech_ms,
        cfg.vad.silence_ms,
        cfg.vad.short_silence_ms,
        cfg.vad.short_utterance_ms,
        cfg.vad.resume_pause_ms,
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
        hard_deadline: None,
        ignore_until: tokio::time::Instant::now(),
        command_ignore_until: tokio::time::Instant::now(),
        max_utterance_ms: cfg.vad.max_utterance_ms.max(1_000),
        generation: 0,
        task: None,
        ui: ui_tx,
        playback,
        youtube,
        alarms: app_alarms,
        pre_roll: VecDeque::new(),
        pre_roll_samples: (cfg.audio.sample_rate as u64 * cfg.vad.activation_pre_roll_ms / 1000)
            as usize,
        volume,
        music: app_music,
        chime,
        commands,
        wake,
        vad,
        frontend,
        history,
        memory,
        turn,
        task_tx,
        started: Instant::now(),
        volume_step: cfg.hardware.volume_step,
        ringing_alarm: None,
    };
    app.enter_idle();

    let mut capture = capture;
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
    tracing::info!("petrovichd {} готов", petrovich_core::VERSION);
    loop {
        let deadline = match (app.follow_deadline, app.hard_deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        };
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
            _ = ticker.tick() => {
                app.tick();
            }
            _ = follow => {
                if app.mode == Mode::Listening {
                    tracing::info!("ожидание речи истекло");
                    if app.dialog.state_chimes {
                        let _ = app.chime.play(ChimeSound::ListeningEnd);
                    }
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
                    let yt = app.youtube.clone();
                    let music = app.music.clone();
                    let text = format!("Таймер {} сработал.", f.label);
                    tokio::spawn(async move {
                        if let Some(m) = &music {
                            m.duck();
                        }
                        if let Some(yt) = &yt {
                            yt.duck();
                        }
                        tokio::time::sleep(Duration::from_millis(1100)).await;
                        if let Err(e) = speak_text(&ctx, &text).await {
                            tracing::warn!("таймер: озвучка: {e:#}");
                        }
                        if let Some(yt) = &yt {
                            yt.unduck();
                        }
                        if let Some(m) = &music {
                            m.unduck();
                        }
                    });
                }
            }
            alarm = alarm_rx.recv() => {
                if let Some(a) = alarm {
                    let _ = app.chime.play(ChimeSound::Alarm);
                    app.ringing_alarm = Some(a.id);
                    let ctx = app.turn.clone();
                    let yt = app.youtube.clone();
                    let music = app.music.clone();
                    let text = if a.label.is_empty() || a.label == "будильник" {
                        format!("Будильник на {:02}:{:02}.", a.hour, a.minute)
                    } else {
                        format!("Будильник «{}» сработал.", a.label)
                    };
                    tokio::spawn(async move {
                        if let Some(m) = &music {
                            m.duck();
                        }
                        if let Some(yt) = &yt {
                            yt.duck();
                        }
                        tokio::time::sleep(Duration::from_millis(1100)).await;
                        if let Err(e) = speak_text(&ctx, &text).await {
                            tracing::warn!("будильник: озвучка: {e:#}");
                        }
                        if let Some(yt) = &yt {
                            yt.unduck();
                        }
                        if let Some(m) = &music {
                            m.unduck();
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
    if let Some(yt) = &app.youtube {
        yt.stop_playback();
    }
    if let Some(music) = &app.music {
        if music.is_playing().unwrap_or(false) {
            let _ = music.pause();
        }
    }
    let _ = std::fs::remove_file(&pid);
    Ok(())
}

impl App {
    /// Раз в секунду обновляет экран позицией воспроизведения. Событие
    /// держится `indication.hold_s`, поэтому без обновления индикатор
    /// вернулся бы к часам во время фильма.
    fn tick(&self) {
        if self.playback.is_playing() {
            return;
        }
        if let Some(yt) = &self.youtube {
            if let Some((pos, dur)) = yt.position_now() {
                let status = yt.status_text().unwrap_or_default();
                let title = status
                    .split_once(':')
                    .map(|(_, t)| t.trim())
                    .unwrap_or(status.as_str())
                    .split(',')
                    .next()
                    .unwrap_or("")
                    .to_owned();
                let _ = self.ui.send(UiEvent::Playback {
                    playing: yt.is_playing(),
                    title,
                    position_s: pos,
                    duration_s: dur,
                });
            }
        }
    }

    fn enter_idle(&mut self) {
        self.mode = Mode::Idle;
        if let Some(yt) = &self.youtube {
            yt.unduck();
        }
        self.follow_deadline = None;
        self.hard_deadline = None;
        self.vad.reset_utterance();
        let _ = self.ui.send(UiEvent::State(VoiceState::Idle));
    }

    fn enter_listening(&mut self, follow_up: bool) {
        self.mode = Mode::Listening;
        self.vad.reset_utterance();
        if follow_up {
            // Хвост озвучки уже слит; 40 мс гасят щелчок, не съедая начало фразы.
            self.ignore_until = tokio::time::Instant::now() + Duration::from_millis(40);
        }
        let ms = if follow_up {
            self.dialog.follow_up_ms
        } else {
            self.dialog.listen_timeout_ms
        };
        let now = tokio::time::Instant::now();
        self.follow_deadline = Some(now + Duration::from_millis(ms));
        // Даже если VAD будет постоянно слышать «речь», сессия закончится:
        // тайм-аут можно продлить не дольше, чем на одну максимальную фразу.
        let cap_ms = ms
            .saturating_add(self.max_utterance_ms)
            .saturating_add(1_000);
        self.hard_deadline = Some(now + Duration::from_millis(cap_ms));
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
        if let Some(yt) = &self.youtube {
            yt.duck();
        }
        let _ = self.ui.send(UiEvent::Notice(UiNotice::Listening));
        if self.dialog.wake_chime {
            let _ = self.chime.play(ChimeSound::Listening);
            // Сигнал короткий; не отрезаем первые слоги сразу после «Петрович».
            self.ignore_until = tokio::time::Instant::now() + Duration::from_millis(40);
            // Свой сигнал не должен распознаваться как «громче»/«стоп».
            self.command_ignore_until = tokio::time::Instant::now() + Duration::from_millis(700);
        }
        let buffered: Vec<Vec<i16>> = self.pre_roll.drain(..).collect();
        let pre_db = buffered
            .iter()
            .map(|b| petrovich_audio::vad::dbfs(b))
            .fold(-120.0f32, f32::max);
        tracing::info!(
            "активация: пре-ролл {} блоков, максимум {pre_db:.1} dBFS",
            buffered.len()
        );
        self.enter_listening(false);
        for samples in buffered {
            let was_in_speech = self.vad.in_speech();
            let utts = self.vad.push(&samples);
            if let Some(d) = listen_deadline(
                tokio::time::Instant::now(),
                self.vad.in_speech(),
                was_in_speech,
                self.hard_deadline,
                self.max_utterance_ms,
                self.dialog.follow_up_ms,
            ) {
                self.follow_deadline = Some(d);
            }
            if let Some(u) = utts.into_iter().next() {
                tracing::info!("фраза из пре-ролла: {} мс, отправляю в STT", u.speech_ms);
                self.start_turn(u.samples);
                return;
            }
        }
    }

    fn remember_pre_roll(&mut self, samples: &[i16]) {
        if self.pre_roll_samples == 0 {
            return;
        }
        self.pre_roll.push_back(samples.to_vec());
        let mut total: usize = self.pre_roll.iter().map(Vec::len).sum();
        while total > self.pre_roll_samples {
            if let Some(front) = self.pre_roll.pop_front() {
                total = total.saturating_sub(front.len());
            } else {
                break;
            }
        }
    }

    fn on_block(&mut self, block: CaptureBlock) {
        let frames = self.frontend.push(&block);
        let now = self.started.elapsed().as_millis() as u64;
        for frame in frames {
            if self.muted || tokio::time::Instant::now() < self.ignore_until {
                continue;
            }
            let commands = if tokio::time::Instant::now() >= self.command_ignore_until {
                match self.commands.as_mut() {
                    Some(spotter) => spotter.feed(&frame.samples),
                    None => Vec::new(),
                }
            } else {
                Vec::new()
            };
            for command in commands {
                // Во время TTS/сигналов не даём озвучке самой переключать
                // трек или громкость. «Стоп» остаётся доступен — им и
                // останавливают ассистента.
                let always = matches!(
                    command,
                    Command::Stop | Command::VolumeUp | Command::VolumeDown
                );
                if self.playback.is_playing() && !always && !self.frontend.echo_cancelled() {
                    tracing::debug!(
                        "быстрая команда {command:?}: нет AEC, пропускаю во время речи"
                    );
                    continue;
                }
                self.on_command(command);
            }
            match self.mode {
                Mode::Idle => {
                    self.remember_pre_roll(&frame.samples);
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
                    let was_in_speech = self.vad.in_speech();
                    let utts = self.vad.push(&frame.samples);
                    if let Some(d) = listen_deadline(
                        tokio::time::Instant::now(),
                        self.vad.in_speech(),
                        was_in_speech,
                        self.hard_deadline,
                        self.max_utterance_ms,
                        self.dialog.follow_up_ms,
                    ) {
                        self.follow_deadline = Some(d);
                    }
                    if !was_in_speech && self.vad.in_speech() {
                        tracing::info!("VAD: речь пошла");
                    } else if was_in_speech && !self.vad.in_speech() {
                        tracing::info!("VAD: речь закончилась");
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
                    if self.dialog.barge_in_voice && self.playback.is_playing() {
                        let _ = self.vad.push(&frame.samples);
                        if self.vad.in_speech() {
                            tracing::info!("бардж-ин по голосу");
                            self.abort_turn();
                            self.playback.stop();
                            self.mode = Mode::Listening;
                            let now = tokio::time::Instant::now();
                            self.follow_deadline =
                                Some(now + Duration::from_millis(self.dialog.follow_up_ms));
                            // VAD уже слышит речь: не сбрасываем буфер, но
                            // ограничиваем сессию, как при обычном слушании.
                            self.hard_deadline = Some(
                                now + Duration::from_millis(
                                    self.dialog
                                        .follow_up_ms
                                        .saturating_add(self.max_utterance_ms)
                                        .saturating_add(1_000),
                                ),
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
        let mut snapshot = self.history.snapshot();
        if let Some(memory) = &self.memory {
            inject_shared(memory, &mut snapshot);
        }
        tracing::info!("ход диалога: STT → LLM → TTS");
        self.turn
            .cancelled
            .store(false, std::sync::atomic::Ordering::Release);
        if let Some(yt) = &self.youtube {
            yt.duck();
        }
        self.mode = Mode::Thinking;
        self.follow_deadline = None;
        self.hard_deadline = None;
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
                        if let Some(yt) = &self.youtube {
                            yt.stop_playback();
                        }
                        self.enter_idle();
                    }
                    None => {
                        // Без финального сигнала: он задерживал следующее
                        // прослушивание и мог попасть в микрофон.
                        if !outcome.spoken.is_empty() {
                            let _ = self.ui.send(UiEvent::Notice(UiNotice::Done));
                        }
                        if let Some(yt) = &self.youtube {
                            yt.unduck();
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
                if let Some(yt) = &self.youtube {
                    yt.unduck();
                }
                self.enter_listening(true);
            }
        }
    }

    fn announce(&self, text: &str) {
        let ctx = self.turn.clone();
        let yt = self.youtube.clone();
        let text = text.to_owned();
        tokio::spawn(async move {
            if let Some(yt) = &yt {
                yt.duck();
            }
            if let Err(e) = speak_text(&ctx, &text).await {
                tracing::warn!("озвучка: {e:#}");
            }
            if let Some(yt) = &yt {
                yt.unduck();
            }
        });
    }

    fn abort_turn(&mut self) {
        self.turn
            .cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        self.generation = self.generation.wrapping_add(1);
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }

    fn change_volume(&mut self, delta: i32) {
        match self.volume.step(delta) {
            Ok(s) => {
                let _ = self.ui.send(UiEvent::Volume(s.percent));
            }
            Err(e) => tracing::warn!("громкость: {e}"),
        }
    }

    fn on_command(&mut self, command: Command) {
        tracing::info!("быстрая команда: {command:?}");
        match command {
            Command::Next => self.music_call(|m| m.next()),
            Command::Previous => self.music_call(|m| m.previous()),
            Command::Play => self.music_call(|m| m.resume()),
            Command::Stop => self.global_stop(),
            Command::VolumeUp => self.change_volume(self.volume_step),
            Command::VolumeDown => self.change_volume(-self.volume_step),
        }
    }

    fn music_call<F>(&self, f: F)
    where
        F: FnOnce(&dyn MusicControl) -> petrovich_core::Result<()>,
    {
        let Some(music) = self.music.as_ref() else {
            tracing::debug!("быстрая команда: музыка не настроена");
            return;
        };
        if let Err(e) = f(music.as_ref()) {
            tracing::warn!("быстрая команда: музыка: {e}");
        }
    }

    /// Глобальная «пауза»/«стоп»/«хватит»: останавливает только то, что
    /// сейчас на переднем плане, а не всё сразу.
    fn global_stop(&mut self) {
        if let Some(id) = self.ringing_alarm.take() {
            let alarms = self.alarms.clone();
            tokio::spawn(async move {
                alarms.dismiss(id).await;
            });
        }
        let youtube_playing = self.youtube.as_ref().is_some_and(|y| y.is_playing());
        let music_playing = self
            .music
            .as_ref()
            .map(|m| m.is_playing().unwrap_or(false))
            .unwrap_or(false);
        match stop_target(self.mode, youtube_playing || music_playing) {
            StopTarget::Assistant => {
                tracing::info!("глобальная пауза: ассистент");
                self.abort_turn();
                self.playback.stop();
                self.feedback(ChimeSound::Stopped, UiNotice::Stopped);
                self.enter_idle();
            }
            StopTarget::Media => {
                tracing::info!("глобальная пауза: музыка/видео");
                let mut failed = false;
                if music_playing {
                    if let Some(music) = self.music.as_ref() {
                        if let Err(e) = music.pause() {
                            tracing::warn!("глобальная пауза: музыка: {e}");
                            failed = true;
                        }
                    }
                }
                if youtube_playing {
                    if let Some(yt) = &self.youtube {
                        if let Err(e) = yt.pause_playback() {
                            tracing::warn!("глобальная пауза: видео: {e:#}");
                            failed = true;
                        }
                    }
                }
                if !failed {
                    self.feedback(ChimeSound::Stopped, UiNotice::Stopped);
                }
            }
            StopTarget::None => tracing::debug!("глобальная пауза: нечего останавливать"),
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
                self.change_volume(delta);
            }
            ButtonEvent::Mute(m) => {
                self.muted = m;
                let _ = self.ui.send(UiEvent::Muted(m));
                self.abort_turn();
                self.playback.stop();
                if let Some(yt) = &self.youtube {
                    yt.stop_playback();
                }
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
    let profile: &'static HardwareProfile = petrovich_core::profile::resolve(&cfg.hardware.model)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("модель: {} ({})", profile.model_name, profile.codename);
    if cfg.wakeword.enabled {
        check_file(&cfg.wakeword.model, "модель активации")?;
        if cfg.wakeword.model.extension().and_then(|e| e.to_str()) == Some("tflite") {
            check_file(&cfg.wakeword.assets, "feature-backbone")?;
            petrovich_wakeword::TfliteHead::load(&cfg.wakeword.model)
                .map_err(|e| anyhow::anyhow!("модель активации: {e:#}"))?;
            let assets = std::fs::read(&cfg.wakeword.assets)
                .map_err(|e| anyhow::anyhow!("{}: {e}", cfg.wakeword.assets.display()))?;
            let tensors = petrovich_wakeword::tensors::read(&assets)
                .map_err(|e| anyhow::anyhow!("backbone: {e:#}"))?;
            if !tensors.contains_key("mel.filters") {
                bail!("в backbone нет mel.filters");
            }
            println!(
                "активация: openWakeWord «{}» ({})",
                cfg.wakeword
                    .model
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("wakeword"),
                cfg.wakeword.model.display()
            );
        } else if petrovich_wakeword::micro::is_micro_manifest(&cfg.wakeword.model) {
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
            petrovich_wakeword::WakeModel::load(&cfg.wakeword.model)
                .map_err(|e| anyhow::anyhow!("модель активации: {e:#}"))?;
            let assets = std::fs::read(&cfg.wakeword.assets)
                .map_err(|e| anyhow::anyhow!("{}: {e}", cfg.wakeword.assets.display()))?;
            let tensors = petrovich_wakeword::tensors::read(&assets)
                .map_err(|e| anyhow::anyhow!("backbone: {e:#}"))?;
            if !tensors.contains_key("mel.filters") {
                bail!("в backbone нет mel.filters");
            }
            println!("активация: {}", cfg.wakeword.model.display());
        }
    }
    if cfg.memory.enabled {
        let path = cfg.memory_path();
        let memory = Memory::load(
            &path,
            cfg.memory.max_entries,
            cfg.memory.max_chars,
            cfg.memory.prompt_chars,
        );
        println!("память: {} фактов ({})", memory.len(), path.display());
    } else {
        println!("память: выключена");
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
    if cfg.audio.vqe != petrovich_core::config::VqeMode::Off {
        match petrovich_audio::vqe::available() {
            Ok(()) => println!("VQE прошивки: доступен"),
            Err(e) => println!("VQE прошивки: недоступен ({e}); будет собственный DSP"),
        }
    }
    if cfg.commands.enabled {
        check_file(&cfg.commands.model, "модель быстрых команд")?;
        CommandSpotter::open(&cfg.commands.model)
            .map_err(|e| anyhow::anyhow!("быстрые команды: {e:#}"))?;
        println!("быстрые команды: {}", cfg.commands.model.display());
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

/// Новый тайм-аут ожидания речи после кадра VAD.
///
/// Во время речи срок продлевается на максимальную длительность фразы, но
/// никогда не снимается полностью: короткий шум не должен оставлять режим
/// `Listening` навсегда. `hard` — общий предел сессии.
fn listen_deadline(
    now: tokio::time::Instant,
    in_speech: bool,
    was_in_speech: bool,
    hard: Option<tokio::time::Instant>,
    max_utterance_ms: u64,
    follow_up_ms: u64,
) -> Option<tokio::time::Instant> {
    let ms = if in_speech {
        max_utterance_ms
    } else if was_in_speech {
        follow_up_ms.max(1_000)
    } else {
        return None;
    };
    let deadline = now + Duration::from_millis(ms);
    Some(match hard {
        Some(cap) => deadline.min(cap),
        None => deadline,
    })
}

#[cfg(test)]
mod deadline_tests {
    use super::*;

    #[test]
    fn short_blip_rearms_follow_up_instead_of_listening_forever() {
        let now = tokio::time::Instant::now();
        let d = listen_deadline(now, false, true, None, 12_000, 5_000)
            .expect("после короткого шума дедлайн должен остаться");
        assert_eq!(d.duration_since(now), Duration::from_millis(5_000));
    }

    #[test]
    fn speech_extension_is_clamped_to_hard_cap() {
        let now = tokio::time::Instant::now();
        let hard = now + Duration::from_millis(6_000);
        let d = listen_deadline(now, true, true, Some(hard), 12_000, 5_000)
            .expect("во время речи дедлайн должен быть");
        assert_eq!(d, hard);
    }

    #[test]
    fn no_speech_keeps_existing_deadline() {
        let now = tokio::time::Instant::now();
        assert!(listen_deadline(now, false, false, None, 12_000, 5_000).is_none());
    }

    #[test]
    fn global_stop_targets_only_the_foreground() {
        use StopTarget::{Assistant, Media, None as Nothing};
        assert_eq!(stop_target(Mode::Thinking, true), Assistant);
        assert_eq!(stop_target(Mode::Thinking, false), Assistant);
        assert_eq!(stop_target(Mode::Listening, true), Media);
        assert_eq!(stop_target(Mode::Listening, false), Assistant);
        assert_eq!(stop_target(Mode::Idle, true), Media);
        assert_eq!(stop_target(Mode::Idle, false), Nothing);
    }
}
