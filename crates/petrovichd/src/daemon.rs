//! Жизненный цикл демона и голосовой автомат.

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

use crate::music::CommandMusic;
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
    max_utterance_ms: u64,
    generation: u64,
    task: Option<JoinHandle<()>>,
    ui: mpsc::UnboundedSender<UiEvent>,
    playback: PlaybackHandle,
    youtube: Arc<YouTubePlayer>,
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
    let timers = TimerManager::new(ui_tx.clone(), announce_tx);
    let alarms = AlarmManager::new(ui_tx.clone(), alarm_tx);
    let chime = Arc::new(LocalChime::new(playback.clone()));
    let search_key = loaded
        .secrets
        .get(&cfg.tools.search_api_key_env)
        .map(str::to_owned)
        .unwrap_or_default();
    let web = Web::with_search_proxy(
        cfg.tools.web_timeout_ms,
        cfg.tools.web_max_bytes,
        cfg.tools.web_allow_private,
        &cfg.tools.search_proxy,
    )?
    .with_search_api(search_key);
    let music: Option<Arc<dyn MusicControl>> = CommandMusic::new(cfg.tools.music_command.clone())
        .map(|m| Arc::new(m) as Arc<dyn MusicControl>);
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
    let youtube = YouTubePlayer::new(&cfg.tools, cfg.audio.playback_pcm.clone())?;
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
    let mut tools = ToolContext::new(
        volume.clone(),
        music,
        timers,
        alarms,
        web,
        chime.clone(),
        ui_tx.clone(),
        cfg.tools.clone(),
        cfg.hardware.volume_step,
    )
    .with_youtube(youtube.clone());
    if !cfg.tools.weather_url.is_empty() {
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
    if !cfg.tools.nightscout_url.is_empty() {
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
    let hardware = petrovich_hw::start(profile, &cfg.hardware, ui_rx);
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
    };
    let vad = petrovich_audio::vad::Vad::new(
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
        hard_deadline: None,
        ignore_until: tokio::time::Instant::now(),
        max_utterance_ms: cfg.vad.max_utterance_ms.max(1_000),
        generation: 0,
        task: None,
        ui: ui_tx,
        playback,
        youtube,
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
                    let text = format!("Таймер {} сработал.", f.label);
                    tokio::spawn(async move {
                        yt.duck();
                        tokio::time::sleep(Duration::from_millis(1100)).await;
                        if let Err(e) = speak_text(&ctx, &text).await {
                            tracing::warn!("таймер: озвучка: {e:#}");
                        }
                        yt.unduck();
                    });
                }
            }
            alarm = alarm_rx.recv() => {
                if let Some(a) = alarm {
                    let _ = app.chime.play(ChimeSound::Alarm);
                    let ctx = app.turn.clone();
                    let yt = app.youtube.clone();
                    let text = if a.label.is_empty() || a.label == "будильник" {
                        format!("Будильник на {:02}:{:02}.", a.hour, a.minute)
                    } else {
                        format!("Будильник «{}» сработал.", a.label)
                    };
                    tokio::spawn(async move {
                        yt.duck();
                        tokio::time::sleep(Duration::from_millis(1100)).await;
                        if let Err(e) = speak_text(&ctx, &text).await {
                            tracing::warn!("будильник: озвучка: {e:#}");
                        }
                        yt.unduck();
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
    app.youtube.stop_playback();
    let _ = std::fs::remove_file(&pid);
    Ok(())
}

impl App {
    fn enter_idle(&mut self) {
        self.mode = Mode::Idle;
        self.youtube.unduck();
        self.follow_deadline = None;
        self.hard_deadline = None;
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
        self.youtube.duck();
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
            let commands = match self.commands.as_mut() {
                Some(spotter) => spotter.feed(&frame.samples),
                None => Vec::new(),
            };
            for command in commands {
                // Во время TTS/сигналов не даём озвучке самой переключать
                // трек или громкость. «Стоп» остаётся доступен — им и
                // останавливают ассистента.
                if self.playback.is_playing() && !matches!(command, Command::Stop) {
                    tracing::debug!("быстрая команда {command:?}: пропускаю во время речи");
                    continue;
                }
                self.on_command(command);
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
        self.youtube.duck();
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
                        self.youtube.stop_playback();
                        self.enter_idle();
                    }
                    None => {
                        if !outcome.spoken.is_empty() {
                            self.feedback(ChimeSound::Done, UiNotice::Done);
                        }
                        self.youtube.unduck();
                        self.enter_listening(true);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("ход диалога: {e}");
                let _ = self.ui.send(UiEvent::Error(e.clone()));
                let _ = self.ui.send(UiEvent::Notice(UiNotice::Error));
                let _ = self.chime.play(ChimeSound::Error);
                self.youtube.unduck();
                self.enter_listening(true);
            }
        }
    }

    fn announce(&self, text: &str) {
        let ctx = self.turn.clone();
        let yt = self.youtube.clone();
        let text = text.to_owned();
        tokio::spawn(async move {
            yt.duck();
            if let Err(e) = speak_text(&ctx, &text).await {
                tracing::warn!("озвучка: {e:#}");
            }
            yt.unduck();
        });
    }

    fn abort_turn(&mut self) {
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
        let youtube_playing = self.youtube.is_playing();
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
                    if let Err(e) = self.youtube.pause_playback() {
                        tracing::warn!("глобальная пауза: видео: {e:#}");
                        failed = true;
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
                self.youtube.stop_playback();
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
