//! YouTube только звуком: без yt-dlp и mpv.
//!
//! Плеер просит у InnerTube HLS-манифест через клиент IOS, забирает
//! аудиодорожку AAC и декодирует её Symphonia. Если HLS у видео нет
//! (подкасты, часть роликов), берётся DASH-аудио MP4 и читается range-
//! запросами, поэтому целый файл в память не грузится. Сеть может идти через
//! byedpi (`tools.youtube_proxy`), а звук — в отдельный клиент `default`
//! (dmix), чтобы не занимать очередь TTS.

use std::collections::VecDeque;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use petrovich_audio::alsa::Pcm;
use petrovich_audio::resample::LinearResampler;
use petrovich_core::config::ToolsConfig;
use petrovich_core::ports::{YouTubeControl, YouTubeDetails, YouTubeHit, YouTubeSearchResult};
use serde_json::{json, Value};
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use tokio::sync::mpsc;

/// Всё, что нужно для управления одним воспроизведением YouTube.
type AudioSession = (
    mpsc::Sender<(u32, Vec<i16>)>,
    Arc<AtomicBool>,
    Arc<AtomicBool>,
    Arc<AtomicBool>,
    Arc<AtomicU64>,
);

/// Открытый DASH-контейнер, декодер и индекс аудиодорожки.
type DashReader = (
    Box<dyn symphonia::core::formats::FormatReader>,
    Box<dyn symphonia::core::codecs::audio::AudioDecoder>,
    u32,
);

const IOS_CLIENT_NAME: &str = "5";
const IOS_CLIENT_VERSION: &str = "20.49.6";
const IOS_UA: &str =
    "com.google.ios.youtube/20.49.6 (iPhone16,2; U; CPU iOS 18_1_0 like Mac OS X;)";
const WEB_CLIENT_NAME: &str = "1";
const WEB_CLIENT_VERSION: &str = "2.20260114.08.00";
const WEB_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";
const API: &str = "https://youtubei.googleapis.com/youtubei/v1";
/// Фронт для media-хостов googlevideo.com: их SNI режут чаще, чем
/// youtubei.googleapis.com. Заголовок Host сохраняет исходный хост.
const FRONT_HOST: &str = "youtubei.googleapis.com";
const MAX_JSON: usize = 8 * 1024 * 1024;
const MAX_SEGMENT: usize = 4 * 1024 * 1024;
/// 100 мс PCM: пауза срабатывает быстро, а очередь остаётся короткой.
const CHUNK_MS: u32 = 100;
const OUT_RATE: u32 = 48_000;
const OUT_PERIOD_MS: u32 = 20;
const MEDIA_TIMEOUT: Duration = Duration::from_secs(6);

fn build_client(cfg: &ToolsConfig, tls12_only: bool) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .user_agent(IOS_UA)
        .http1_only()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30));
    if tls12_only {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls = rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS12])
            .with_root_certificates(roots)
            .with_no_client_auth();
        builder = builder.use_preconfigured_tls(tls);
    }
    let proxy = cfg.youtube_proxy.trim();
    if !proxy.is_empty() {
        builder = builder
            .proxy(reqwest::Proxy::all(proxy).with_context(|| format!("youtube_proxy {proxy}"))?);
    }
    builder.build().context("HTTP-клиент YouTube")
}

fn core_err(e: anyhow::Error) -> petrovich_core::Error {
    petrovich_core::Error::Hardware(format!("{e:#}"))
}

fn write_json_atomic<T: serde::Serialize>(path: &std::path::Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec_pretty(value)?;
    let tmp = path.with_extension("json.tmp");
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    let mut f = options.open(&tmp)?;
    f.write_all(&data)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

struct Session {
    title: String,
    video_id: String,
    query: String,
    /// Задача потока; при замене трека её abort() освобождает сеть.
    paused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    ended: Arc<AtomicBool>,
    user_paused: bool,
    /// Позиция и длительность в миллисекундах: обновляются потоком вывода.
    position_ms: Arc<AtomicU64>,
    duration_ms: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<()>,
}

/// Сохранённая позиция, чтобы продолжить после перезапуска.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct SavedPosition {
    #[serde(default)]
    video_id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    position_ms: u64,
    #[serde(default)]
    duration_ms: u64,
}

pub struct YouTubePlayer {
    client: reqwest::Client,
    /// Запасной клиент только с TLS 1.2: ТСПУ иногда режет TLS 1.3
    /// с постквантовым Kyber в ClientHello.
    tls12: reqwest::Client,
    playback_pcm: String,
    /// Прокси для DASH range-запросов; blocking-клиент создаётся
    /// в потоке декодера, а не в async-рантайме.
    proxy: String,
    /// Демон приглушает звук на время хода/объявления; новые сессии
    /// стартуют уже приглушёнными.
    ducked: AtomicBool,
    session: Mutex<Option<Session>>,
    queue: Mutex<VecDeque<String>>,
    state_path: Option<PathBuf>,
    me: OnceLock<Weak<YouTubePlayer>>,
}

impl YouTubePlayer {
    pub fn new(cfg: &ToolsConfig, playback_pcm: String) -> Result<Arc<Self>> {
        let player = Arc::new(Self {
            client: build_client(cfg, false)?,
            tls12: build_client(cfg, true)?,
            playback_pcm,
            proxy: cfg.youtube_proxy.trim().to_owned(),
            ducked: AtomicBool::new(false),
            session: Mutex::new(None),
            queue: Mutex::new(VecDeque::new()),
            state_path: None,
            me: OnceLock::new(),
        });
        let _ = player.me.set(Arc::downgrade(&player));
        Ok(player)
    }

    /// Включает сохранение позиции между перезапусками.
    pub fn with_state_path(mut self: Arc<Self>, path: PathBuf) -> Arc<Self> {
        if let Some(this) = Arc::get_mut(&mut self) {
            this.state_path = Some(path);
        }
        self
    }

    /// Приглушить на время ответа или объявления. Не отменяет паузу,
    /// поставленную пользователем: звук продолжает идти тише.
    pub fn duck(&self) {
        self.ducked.store(true, Ordering::Release);
    }

    pub fn unduck(&self) {
        self.ducked.store(false, Ordering::Release);
    }

    fn drop_session(&self) {
        if let Ok(mut guard) = self.session.lock() {
            if let Some(old) = guard.take() {
                old.stop.store(true, Ordering::Release);
                old.task.abort();
            }
        }
    }

    async fn start_with(
        &self,
        client: &reqwest::Client,
        query: &str,
    ) -> Result<(String, String, String, u64, Media)> {
        let (video_id, mut title, mut author) = match extract_video_id(query) {
            Some(id) => (id, String::new(), String::new()),
            None => search(client, query).await?,
        };
        let (duration_ms, stream) = match video_info(client, &video_id).await {
            Ok(info) => {
                if title.is_empty() {
                    title = info.title;
                    author = info.author;
                }
                (info.duration_ms, info.stream)
            }
            Err(e) => return Err(e),
        };
        if title.is_empty() {
            title = video_id.clone();
        }
        let media = match stream {
            Stream::Hls(master) => Media::Hls(audio_segments(client, &master).await?),
            Stream::Dash(dash) => Media::Dash(dash),
        };
        Ok((video_id, title, author, duration_ms, media))
    }

    /// Возвращает клиент, которым реально удалось разрешить видео:
    /// если TLS 1.3 не проходит, весь запрос повторяется через TLS 1.2.
    async fn start(
        &self,
        query: &str,
    ) -> Result<(String, String, String, u64, Media, reqwest::Client)> {
        let primary = self.client.clone();
        match self.start_with(&primary, query).await {
            Ok((id, title, author, duration, media)) => {
                Ok((id, title, author, duration, media, primary))
            }
            Err(e1) => {
                tracing::info!("youtube: TLS 1.3 не сработал ({e1:#}), повторяю через TLS 1.2");
                let fallback = self.tls12.clone();
                match self.start_with(&fallback, query).await {
                    Ok((id, title, author, duration, media)) => {
                        Ok((id, title, author, duration, media, fallback))
                    }
                    Err(e2) => Err(e1.context(format!("TLS 1.2 тоже не сработал: {e2:#}"))),
                }
            }
        }
    }
}

impl YouTubePlayer {
    pub async fn play_impl(&self, query: &str) -> Result<String> {
        self.play_impl_at(query, 0).await
    }

    /// Запускает ролик с указанной позиции. Перемотка перезапускает поток:
    /// это надёжнее, чем пытаться подвинуть уже декодируемый HLS, и не
    /// держит в памяти второй декодер.
    async fn play_impl_at(&self, query: &str, start_ms: u64) -> Result<String> {
        self.save_position();
        let (video_id, title, author, reported_duration, media, client) = self.start(query).await?;
        match media {
            Media::Hls(segments) => {
                let start = segments_after(&segments, start_ms);
                let first = segments
                    .get(start)
                    .or_else(|| segments.first())
                    .ok_or_else(|| anyhow::anyhow!("YouTube: пустой HLS-плейлист"))?;
                // Проверяем первый сегмент до обещания «включаю»: иначе при
                // заблокированном googlevideo ассистент говорит это в тишину.
                let first_bytes = fetch_segment(&client, &first.url).await?;
                let rate =
                    tokio::task::spawn_blocking(move || decode_adts(&first_bytes).map(|(r, _)| r))
                        .await
                        .context("youtube: декодер")??;
                tracing::debug!("youtube: первый сегмент декодирован @ {rate} Гц");
                let duration = reported_duration.max(segments.iter().map(|s| s.duration_ms).sum());
                let query = query.to_owned();
                let start_offset = segments
                    .iter()
                    .take(start)
                    .map(|s| s.duration_ms)
                    .sum::<u64>()
                    .max(start_ms.min(duration));
                let stream_id = video_id.clone();
                self.start_track(
                    title,
                    author,
                    video_id,
                    query,
                    start_offset,
                    duration,
                    move |tx, stop, position| {
                        tokio::spawn(async move {
                            stream_hls(client, stream_id, segments, start, tx, stop, position)
                                .await;
                        })
                    },
                )
            }
            Media::Dash(dash) => {
                self.start_dash_track(
                    title,
                    author,
                    video_id,
                    query.to_owned(),
                    reported_duration,
                    dash,
                    start_ms,
                )
                .await
            }
        }
    }

    /// Запускает поток вывода и фоновую загрузку/декодирование, затем
    /// публикует сессию. Ошибка до этого места не трогает текущий трек.
    #[allow(clippy::too_many_arguments)]
    fn start_track<F>(
        &self,
        title: String,
        author: String,
        video_id: String,
        query: String,
        start_ms: u64,
        duration_ms: u64,
        spawn: F,
    ) -> Result<String>
    where
        F: FnOnce(
            mpsc::Sender<(u32, Vec<i16>)>,
            Arc<AtomicBool>,
            Arc<AtomicU64>,
        ) -> tokio::task::JoinHandle<()>,
    {
        self.drop_session();
        let (tx, paused, stop, ended, position) = self.prepare_audio(start_ms)?;
        let task_ended = Arc::clone(&ended);
        let inner = spawn(tx, Arc::clone(&stop), Arc::clone(&position));
        let task = tokio::spawn(async move {
            let _ = inner.await;
            task_ended.store(true, Ordering::Release);
        });
        Ok(self.install_session(
            title,
            author,
            video_id,
            query,
            start_ms,
            duration_ms,
            paused,
            stop,
            ended,
            position,
            task,
        ))
    }

    /// DASH не проверяем отдельным запросом: поток сам сигналит, когда
    /// декодировал первый пакет. Так одна range-сессия, а не две.
    #[allow(clippy::too_many_arguments)]
    async fn start_dash_track(
        &self,
        title: String,
        author: String,
        video_id: String,
        query: String,
        duration_ms: u64,
        dash: DashAudio,
        start_ms: u64,
    ) -> Result<String> {
        let (tx, paused, stop, ended, position) = self.prepare_audio(start_ms)?;
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let proxy = self.proxy.clone();
        let task_stop = Arc::clone(&stop);
        let task_ended = Arc::clone(&ended);
        let position_task = Arc::clone(&position);
        let task = tokio::spawn(async move {
            let joined = tokio::task::spawn_blocking(move || {
                decode_dash(
                    &proxy,
                    &dash.url,
                    dash.len,
                    start_ms,
                    tx,
                    &task_stop,
                    &position_task,
                    Some(ready_tx),
                )
            })
            .await;
            match joined {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!("youtube: DASH: {e:#}"),
                Err(e) => tracing::warn!("youtube: DASH-поток: {e}"),
            }
            task_ended.store(true, Ordering::Release);
        });
        match tokio::time::timeout(Duration::from_secs(12), ready_rx).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => {
                stop.store(true, Ordering::Release);
                task.abort();
                return Err(e.context("youtube: DASH"));
            }
            Ok(Err(e)) => {
                stop.store(true, Ordering::Release);
                task.abort();
                bail!("youtube: DASH-поток: {e}");
            }
            Err(_) => {
                stop.store(true, Ordering::Release);
                task.abort();
                bail!("youtube: DASH не начал играть за 12 с");
            }
        }
        self.drop_session();
        Ok(self.install_session(
            title,
            author,
            video_id,
            query,
            start_ms,
            duration_ms,
            paused,
            stop,
            ended,
            position,
            task,
        ))
    }

    fn prepare_audio(&self, start_ms: u64) -> Result<AudioSession> {
        // Пауза — только пользовательская. Приглушение ассистента меняет
        // амплитуду в audio_thread, не останавливая воспроизведение.
        let paused = Arc::new(AtomicBool::new(false));
        let ducked = Arc::clone(&self.ducked);
        let stop = Arc::new(AtomicBool::new(false));
        let ended = Arc::new(AtomicBool::new(false));
        let position = Arc::new(AtomicU64::new(start_ms));
        let (tx, rx) = mpsc::channel::<(u32, Vec<i16>)>(8);
        let pcm_name = self.playback_pcm.clone();
        let audio_paused = Arc::clone(&paused);
        let audio_ducked = Arc::clone(&ducked);
        let audio_stop = Arc::clone(&stop);
        let audio_position = Arc::clone(&position);
        thread::Builder::new()
            .name("petrovich-youtube".into())
            .spawn(move || {
                audio_thread(
                    rx,
                    audio_paused,
                    audio_ducked,
                    audio_stop,
                    audio_position,
                    &pcm_name,
                )
            })
            .context("поток вывода YouTube")?;
        Ok((tx, paused, stop, ended, position))
    }

    #[allow(clippy::too_many_arguments)]
    fn install_session(
        &self,
        title: String,
        author: String,
        video_id: String,
        query: String,
        start_ms: u64,
        duration_ms: u64,
        paused: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        ended: Arc<AtomicBool>,
        position_ms: Arc<AtomicU64>,
        task: tokio::task::JoinHandle<()>,
    ) -> String {
        if let Ok(mut guard) = self.session.lock() {
            *guard = Some(Session {
                title: title.clone(),
                video_id: video_id.clone(),
                query,
                paused,
                stop,
                ended,
                user_paused: false,
                position_ms,
                duration_ms: Arc::new(AtomicU64::new(duration_ms)),
                task,
            });
        }
        self.watch_end(video_id);
        let mut out = format!("Включаю: {title}");
        if !author.is_empty() {
            out.push_str(&format!(" — {author}"));
        }
        if start_ms > 0 {
            out.push_str(&format!(
                " с {}:{:02}",
                start_ms / 60_000,
                start_ms / 1000 % 60
            ));
        }
        out
    }

    /// Следит за концом трека и запускает очередь. Слабый указатель не
    /// создаёт цикл: плеер может быть выгружен вместе с демоном.
    fn watch_end(&self, video_id: String) {
        let Some(me) = self.me.get().cloned() else {
            return;
        };
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(400)).await;
                let Some(player) = me.upgrade() else { return };
                let (ended, stopped) = {
                    let Ok(guard) = player.session.lock() else {
                        return;
                    };
                    let Some(s) = guard.as_ref() else { return };
                    if s.video_id != video_id {
                        return;
                    }
                    (
                        s.ended.load(Ordering::Acquire),
                        s.stop.load(Ordering::Acquire),
                    )
                };
                if stopped {
                    return;
                }
                if !ended {
                    continue;
                }
                player.on_track_end().await;
                return;
            }
        });
    }

    async fn on_track_end(&self) {
        self.save_position();
        let next = self.queue.lock().ok().and_then(|mut q| q.pop_front());
        if let Some(next) = next {
            tracing::info!("youtube: очередь: {next}");
            if let Err(e) = self.play_impl(&next).await {
                tracing::warn!("youtube: очередь: {e:#}");
            }
        } else {
            self.stop_playback();
        }
    }

    /// Играет ли видео прямо сейчас (не пауза и не конец). Нужно глобальной
    /// «паузе», чтобы не останавливать ассистента поверх YouTube.
    pub fn is_playing(&self) -> bool {
        let Ok(guard) = self.session.lock() else {
            return false;
        };
        guard
            .as_ref()
            .is_some_and(|s| !s.ended.load(Ordering::Acquire) && !s.paused.load(Ordering::Acquire))
    }

    /// Сохраняет текущую позицию на диск. Ошибки не фатальны: при
    /// следующем запуске позиции просто не будет.
    fn save_position(&self) {
        let Some(path) = &self.state_path else { return };
        let saved = {
            let Ok(guard) = self.session.lock() else {
                return;
            };
            let Some(s) = guard.as_ref() else { return };
            SavedPosition {
                video_id: s.video_id.clone(),
                title: s.title.clone(),
                position_ms: s.position_ms.load(Ordering::Relaxed),
                duration_ms: s.duration_ms.load(Ordering::Relaxed),
            }
        };
        if let Err(e) = write_json_atomic(path, &saved) {
            tracing::warn!("youtube: позиция: {e:#}");
        }
    }

    fn load_saved(&self) -> Option<SavedPosition> {
        let path = self.state_path.as_ref()?;
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Перезапускает текущий ролик с новой позиции. Делается в фоне: trait
    /// управления синхронный, а разрешение потока требует сети.
    fn restart_at(&self, target_ms: u64) -> Result<()> {
        let (query, title) = {
            let guard = self
                .session
                .lock()
                .map_err(|_| anyhow::anyhow!("youtube: lock"))?;
            let Some(s) = guard.as_ref() else {
                bail!("YouTube: ничего не играет");
            };
            (s.query.clone(), s.title.clone())
        };
        let target = target_ms.min(86_400_000);
        tracing::info!("youtube: перемотка «{title}» на {target} мс");
        let Some(me) = self.me.get().cloned() else {
            bail!("youtube: плеер недоступен");
        };
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Some(player) = me.upgrade() {
                if let Err(e) = player.play_impl_at(&query, target).await {
                    tracing::warn!("youtube: перемотка: {e:#}");
                }
            }
        });
        Ok(())
    }

    fn queue_impl(&self, query: &str) -> String {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        q.push_back(query.to_owned());
        format!("Добавил в очередь: {query}. Всего в очереди: {}.", q.len())
    }

    fn sleep_impl(&self, minutes: u64) -> Result<()> {
        let Some(me) = self.me.get().cloned() else {
            bail!("youtube: плеер недоступен");
        };
        let minutes = minutes.clamp(1, 24 * 60);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(minutes * 60)).await;
            if let Some(player) = me.upgrade() {
                tracing::info!("youtube: таймер сна на {minutes} мин");
                player.stop_playback();
                if let Ok(mut q) = player.queue.lock() {
                    q.clear();
                }
            }
        });
        Ok(())
    }

    pub fn pause_playback(&self) -> Result<()> {
        let mut guard = self
            .session
            .lock()
            .map_err(|_| anyhow::anyhow!("youtube: lock"))?;
        if let Some(s) = guard.as_mut() {
            s.user_paused = true;
            s.paused.store(true, Ordering::Release);
        }
        drop(guard);
        self.save_position();
        Ok(())
    }

    pub fn resume_playback(&self) -> Result<()> {
        let mut guard = self
            .session
            .lock()
            .map_err(|_| anyhow::anyhow!("youtube: lock"))?;
        if let Some(s) = guard.as_mut() {
            s.user_paused = false;
            s.paused.store(false, Ordering::Release);
        }
        Ok(())
    }

    pub fn stop_playback(&self) {
        self.save_position();
        self.drop_session();
    }

    pub fn status_text(&self) -> Result<String> {
        let guard = self
            .session
            .lock()
            .map_err(|_| anyhow::anyhow!("youtube: lock"))?;
        let Some(s) = guard.as_ref() else {
            return Ok("Ничего не играет.".into());
        };
        if s.ended.load(Ordering::Acquire) {
            return Ok("Ничего не играет.".into());
        }
        let pos = s.position_ms.load(Ordering::Relaxed) / 1000;
        let dur = s.duration_ms.load(Ordering::Relaxed) / 1000;
        let at = if dur > 0 {
            format!(", {pos}:{:02} из {}:{:02}", pos % 60, dur / 60, dur % 60)
        } else {
            String::new()
        };
        if s.paused.load(Ordering::Acquire) {
            Ok(format!("Пауза: {}{at}", s.title))
        } else {
            Ok(format!("Играет: {}{at}", s.title))
        }
    }

    /// Текущая позиция для экрана.
    pub fn position_now(&self) -> Option<(u64, u64)> {
        let guard = self.session.lock().ok()?;
        let s = guard.as_ref()?;
        if s.ended.load(Ordering::Acquire) {
            return None;
        }
        Some((
            s.position_ms.load(Ordering::Relaxed) / 1000,
            s.duration_ms.load(Ordering::Relaxed) / 1000,
        ))
    }
}

#[async_trait::async_trait]
#[async_trait::async_trait]
impl YouTubeControl for YouTubePlayer {
    async fn play(&self, query: &str) -> petrovich_core::Result<String> {
        self.play_impl(query).await.map_err(core_err)
    }

    async fn play_video(&self, video_id: &str) -> petrovich_core::Result<String> {
        self.play_impl(video_id).await.map_err(core_err)
    }

    async fn search(
        &self,
        query: &str,
        limit: usize,
        page: Option<&str>,
    ) -> petrovich_core::Result<YouTubeSearchResult> {
        let client = self.client.clone();
        search_page(&client, query, limit, page)
            .await
            .map_err(core_err)
    }

    async fn details(&self, video_id: &str) -> petrovich_core::Result<YouTubeDetails> {
        let client = self.client.clone();
        fetch_details(&client, video_id).await.map_err(core_err)
    }

    fn pause(&self) -> petrovich_core::Result<()> {
        self.pause_playback().map_err(core_err)
    }

    fn resume(&self) -> petrovich_core::Result<()> {
        self.resume_playback().map_err(core_err)
    }

    fn stop(&self) -> petrovich_core::Result<()> {
        self.stop_playback();
        Ok(())
    }

    fn status(&self) -> petrovich_core::Result<String> {
        self.status_text().map_err(core_err)
    }

    fn seek_by(&self, delta_ms: i64) -> petrovich_core::Result<()> {
        let current = self.position_now().map(|(pos, _)| pos * 1000).unwrap_or(0);
        let target = if delta_ms < 0 {
            current.saturating_sub(delta_ms.unsigned_abs())
        } else {
            current.saturating_add(delta_ms as u64)
        };
        self.restart_at(target).map(|_| ()).map_err(core_err)
    }

    fn seek_to(&self, position_ms: u64) -> petrovich_core::Result<()> {
        self.restart_at(position_ms).map(|_| ()).map_err(core_err)
    }

    async fn queue(&self, query: &str) -> petrovich_core::Result<String> {
        Ok(self.queue_impl(query))
    }

    fn sleep_timer(&self, minutes: u64) -> petrovich_core::Result<()> {
        self.sleep_impl(minutes).map_err(core_err)
    }

    fn is_playing(&self) -> bool {
        self.is_playing()
    }

    fn duck(&self) {
        self.duck();
    }

    fn unduck(&self) {
        self.unduck();
    }

    fn position(&self) -> Option<(u64, u64)> {
        self.position_now()
    }

    async fn resume_saved(&self) -> petrovich_core::Result<String> {
        let saved = self
            .load_saved()
            .ok_or_else(|| core_err(anyhow::anyhow!("нет сохранённой позиции YouTube")))?;
        if saved.video_id.is_empty() {
            return Err(core_err(anyhow::anyhow!("нет сохранённой позиции YouTube")));
        }
        self.play_impl_at(&saved.video_id, saved.position_ms)
            .await
            .map_err(core_err)
    }
}

#[derive(Clone)]
struct DashAudio {
    url: String,
    /// Полный размер MP4: нужен Symphonia, чтобы читать диапазоны.
    len: u64,
}

enum Stream {
    Hls(String),
    Dash(DashAudio),
}

#[derive(Clone, Debug)]
struct Segment {
    url: String,
    duration_ms: u64,
}

enum Media {
    Hls(Vec<Segment>),
    Dash(DashAudio),
}

struct VideoInfo {
    title: String,
    author: String,
    duration_ms: u64,
    stream: Stream,
}

async fn innertube(client: &reqwest::Client, path: &str, body: Value, ios: bool) -> Result<Value> {
    let (name, version, ua) = if ios {
        (IOS_CLIENT_NAME, IOS_CLIENT_VERSION, IOS_UA)
    } else {
        (WEB_CLIENT_NAME, WEB_CLIENT_VERSION, WEB_UA)
    };
    let url = format!("{API}/{path}?prettyPrint=false");
    let resp = client
        .post(&url)
        .header("X-YouTube-Client-Name", name)
        .header("X-YouTube-Client-Version", version)
        .header("User-Agent", ua)
        .json(&body)
        .send()
        .await
        .with_context(|| path.to_owned())?;
    let status = resp.status();
    if !status.is_success() {
        bail!("YouTube {path}: HTTP {status}");
    }
    let bytes = resp.bytes().await.context("тело ответа YouTube")?;
    if bytes.len() > MAX_JSON {
        bail!("YouTube {path}: ответ больше {MAX_JSON} байт");
    }
    serde_json::from_slice(&bytes).with_context(|| format!("YouTube {path}: JSON"))
}

async fn search(client: &reqwest::Client, query: &str) -> Result<(String, String, String)> {
    let body = json!({
        "context": {"client": {
            "clientName": "WEB",
            "clientVersion": WEB_CLIENT_VERSION,
            "hl": "ru",
            "gl": "RU"
        }},
        "query": query
    });
    let v = innertube(client, "search", body, false).await?;
    first_video(&v).ok_or_else(|| anyhow::anyhow!("YouTube: ничего не найдено"))
}

/// Поиск с метаданными и продолжением. InnerTube отдаёт до 20 роликов на
/// страницу; `page` — токен предыдущего ответа, а не номер.
async fn search_page(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
    page: Option<&str>,
) -> Result<YouTubeSearchResult> {
    let body = if let Some(token) = page {
        json!({
            "context": {"client": {
                "clientName": "WEB",
                "clientVersion": WEB_CLIENT_VERSION,
                "hl": "ru",
                "gl": "RU"
            }},
            "continuation": token
        })
    } else {
        json!({
            "context": {"client": {
                "clientName": "WEB",
                "clientVersion": WEB_CLIENT_VERSION,
                "hl": "ru",
                "gl": "RU"
            }},
            "query": query
        })
    };
    let v = innertube(client, "search", body, false).await?;
    let mut renderers = Vec::new();
    collect_video_renderers(&v, 0, &mut renderers);
    let mut hits = Vec::with_capacity(renderers.len());
    for r in renderers {
        if hits.len() >= limit.clamp(1, 50) {
            break;
        }
        if let Some(hit) = video_hit(r) {
            if !hits.iter().any(|h: &YouTubeHit| h.id == hit.id) {
                hits.push(hit);
            }
        }
    }
    let estimated_results = v
        .get("estimatedResults")
        .and_then(|x| {
            x.as_str()
                .and_then(|s| s.parse().ok())
                .or_else(|| x.as_u64())
        })
        .unwrap_or(0);
    Ok(YouTubeSearchResult {
        hits,
        continuation: find_continuation(&v).unwrap_or_default(),
        estimated_results,
    })
}

fn collect_video_renderers<'a>(v: &'a Value, depth: u8, out: &mut Vec<&'a Value>) {
    if depth > 14 {
        return;
    }
    if let Some(r) = v.get("videoRenderer") {
        out.push(r);
    }
    match v {
        Value::Array(items) => {
            for x in items {
                collect_video_renderers(x, depth + 1, out);
            }
        }
        Value::Object(map) => {
            for x in map.values() {
                collect_video_renderers(x, depth + 1, out);
            }
        }
        _ => {}
    }
}

fn video_hit(r: &Value) -> Option<YouTubeHit> {
    let id = r.get("videoId")?.as_str()?.to_owned();
    if id.len() != 11 {
        return None;
    }
    let text = |pointer: &str| {
        r.pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let runs_text = |key: &str| {
        r.get(key)
            .and_then(|x| x.get("runs"))
            .and_then(Value::as_array)
            .map(|runs| {
                runs.iter()
                    .filter_map(|x| x.get("text").and_then(Value::as_str))
                    .collect::<String>()
            })
            .unwrap_or_default()
    };
    let mut description = runs_text("detailedMetadataSnippets");
    if description.is_empty() {
        if let Some(snips) = r.get("detailedMetadataSnippets").and_then(Value::as_array) {
            description = snips
                .iter()
                .filter_map(|s| {
                    s.pointer("/snippetText/runs")
                        .and_then(Value::as_array)
                        .map(|runs| {
                            runs.iter()
                                .filter_map(|x| x.get("text").and_then(Value::as_str))
                                .collect::<String>()
                        })
                })
                .collect::<Vec<_>>()
                .join(" ");
        }
    }
    let live = r
        .get("badges")
        .and_then(Value::as_array)
        .is_some_and(|badges| {
            badges.iter().any(|b| {
                b.pointer("/metadataBadgeRenderer/style")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.contains("LIVE"))
            })
        })
        || r.get("lengthText").is_none();
    let title = runs_text("title");
    let title = if title.is_empty() {
        text("/title/simpleText")
    } else {
        title
    };
    let channel = runs_text("ownerText");
    let channel = if channel.is_empty() {
        runs_text("longBylineText")
    } else {
        channel
    };
    Some(YouTubeHit {
        id,
        title: if title.is_empty() {
            "YouTube".into()
        } else {
            title
        },
        channel,
        duration_s: parse_duration(&text("/lengthText/simpleText")),
        published: text("/publishedTimeText/simpleText"),
        description: description.chars().take(500).collect(),
        live,
        views: text("/viewCountText/simpleText"),
    })
}

fn find_continuation(v: &Value) -> Option<String> {
    fn walk(v: &Value, depth: u8, found: &mut Option<String>) {
        if depth > 16 {
            return;
        }
        if let Some(token) = v
            .pointer("/continuationCommand/token")
            .and_then(Value::as_str)
        {
            *found = Some(token.to_owned());
        }
        match v {
            Value::Array(items) => {
                for x in items {
                    walk(x, depth + 1, found);
                }
            }
            Value::Object(map) => {
                for x in map.values() {
                    walk(x, depth + 1, found);
                }
            }
            _ => {}
        }
    }
    let mut found = None;
    walk(v, 0, &mut found);
    found
}

fn parse_duration(text: &str) -> u64 {
    let mut secs = 0u64;
    let mut parts = 0;
    for p in text.trim().split(':') {
        let Ok(n) = p.parse::<u64>() else {
            return 0;
        };
        secs = secs * 60 + n;
        parts += 1;
    }
    if parts == 0 {
        0
    } else {
        secs
    }
}

/// Подробности: описание, субтитры и комментарии (best-effort).
async fn fetch_details(client: &reqwest::Client, video_id: &str) -> Result<YouTubeDetails> {
    let body = json!({
        "context": {"client": {
            "clientName": "IOS",
            "clientVersion": IOS_CLIENT_VERSION,
            "hl": "ru",
            "gl": "RU",
            "userAgent": IOS_UA
        }},
        "videoId": video_id,
        "contentCheckOk": true,
        "racyCheckOk": true
    });
    let v = innertube(client, "player", body, true).await?;
    let title = v
        .pointer("/videoDetails/title")
        .and_then(Value::as_str)
        .unwrap_or("YouTube")
        .to_owned();
    let channel = v
        .pointer("/videoDetails/author")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let length = v
        .pointer("/videoDetails/lengthSeconds")
        .and_then(|x| {
            x.as_str()
                .and_then(|s| s.parse().ok())
                .or_else(|| x.as_u64())
        })
        .unwrap_or(0);
    let description = v
        .pointer("/videoDetails/shortDescription")
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(2000)
        .collect::<String>();
    let subtitles = fetch_subtitles(client, &v).await.unwrap_or_default();
    let comments = fetch_comments(client, video_id).await.unwrap_or_default();
    Ok(YouTubeDetails {
        hit: YouTubeHit {
            id: video_id.to_owned(),
            title,
            channel,
            duration_s: length,
            description,
            ..YouTubeHit::default()
        },
        subtitles,
        comments,
    })
}

async fn fetch_subtitles(client: &reqwest::Client, v: &Value) -> Result<String> {
    let tracks = v
        .pointer("/captions/playerCaptionsTracklistRenderer/captionTracks")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("нет субтитров"))?;
    let track = tracks
        .iter()
        .find(|t| t.get("languageCode").and_then(Value::as_str) == Some("ru"))
        .or_else(|| tracks.first())
        .ok_or_else(|| anyhow::anyhow!("нет субтитров"))?;
    let base = track
        .get("baseUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("нет URL субтитров"))?;
    let url = format!("{base}&fmt=json3");
    let text = fetch_text(client, &url, "субтитры").await?;
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let mut out = String::new();
    if let Some(events) = v.get("events").and_then(Value::as_array) {
        for e in events {
            if let Some(segs) = e.get("segs").and_then(Value::as_array) {
                for s in segs {
                    if let Some(t) = s.get("utf8").and_then(Value::as_str) {
                        if out.chars().count() > 3000 {
                            break;
                        }
                        out.push_str(t);
                    }
                }
                out.push(' ');
            }
        }
    }
    Ok(out.split_whitespace().collect::<Vec<_>>().join(" "))
}

async fn fetch_comments(client: &reqwest::Client, video_id: &str) -> Result<String> {
    let body = json!({
        "context": {"client": {
            "clientName": "WEB",
            "clientVersion": WEB_CLIENT_VERSION,
            "hl": "ru",
            "gl": "RU"
        }},
        "videoId": video_id
    });
    let mut v = innertube(client, "next", body, false).await?;
    let mut out = collect_comments(&v);
    if out.is_empty() {
        if let Some(token) = find_continuation(&v) {
            let body = json!({
                "context": {"client": {
                    "clientName": "WEB",
                    "clientVersion": WEB_CLIENT_VERSION,
                    "hl": "ru",
                    "gl": "RU"
                }},
                "continuation": token
            });
            v = innertube(client, "next", body, false).await?;
            out = collect_comments(&v);
            if out.is_empty() {
                if let Some(token) = find_continuation(&v) {
                    let body = json!({
                        "context": {"client": {
                            "clientName": "WEB",
                            "clientVersion": WEB_CLIENT_VERSION,
                            "hl": "ru",
                            "gl": "RU"
                        }},
                        "continuation": token
                    });
                    let page = innertube(client, "next", body, false).await?;
                    out = collect_comments(&page);
                }
            }
        }
    }
    Ok(out)
}

fn collect_comments(v: &Value) -> String {
    fn walk(v: &Value, depth: u8, out: &mut Vec<String>) {
        if depth > 16 || out.len() >= 6 {
            return;
        }
        if let Some(c) = v.get("commentRenderer") {
            let author = c
                .pointer("/authorText/runs/0/text")
                .and_then(Value::as_str)
                .unwrap_or("");
            let text = c
                .pointer("/contentText/runs")
                .and_then(Value::as_array)
                .map(|runs| {
                    runs.iter()
                        .filter_map(|x| x.get("text").and_then(Value::as_str))
                        .collect::<String>()
                })
                .unwrap_or_default();
            if !text.is_empty() {
                out.push(format!(
                    "{author}: {}",
                    text.chars().take(220).collect::<String>()
                ));
            }
        }
        match v {
            Value::Array(items) => {
                for x in items {
                    walk(x, depth + 1, out);
                }
            }
            Value::Object(map) => {
                for x in map.values() {
                    walk(x, depth + 1, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(v, 0, &mut out);
    out.join(" | ")
}

async fn video_info(client: &reqwest::Client, video_id: &str) -> Result<VideoInfo> {
    let body = json!({
        "context": {"client": {
            "clientName": "IOS",
            "clientVersion": IOS_CLIENT_VERSION,
            "deviceMake": "Apple",
            "deviceModel": "iPhone16,2",
            "osName": "iPhone",
            "osVersion": "18.1.0.22B83",
            "hl": "en",
            "gl": "US",
            "userAgent": IOS_UA
        }},
        "videoId": video_id,
        "contentCheckOk": true,
        "racyCheckOk": true
    });
    let v = innertube(client, "player", body, true).await?;
    let status = v
        .pointer("/playabilityStatus/status")
        .and_then(Value::as_str)
        .unwrap_or("");
    if status != "OK" {
        let reason = v
            .pointer("/playabilityStatus/reason")
            .and_then(Value::as_str)
            .unwrap_or("видео недоступно");
        bail!("YouTube: {status}: {reason}");
    }
    let title = v
        .pointer("/videoDetails/title")
        .and_then(Value::as_str)
        .unwrap_or("YouTube")
        .trim()
        .to_owned();
    let author = v
        .pointer("/videoDetails/author")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    let stream = if let Some(hls) = v
        .pointer("/streamingData/hlsManifestUrl")
        .and_then(Value::as_str)
    {
        Stream::Hls(hls.to_owned())
    } else if let Some(dash) = pick_dash_audio(&v) {
        Stream::Dash(dash)
    } else {
        bail!("YouTube: нет ни HLS, ни DASH-аудио");
    };
    let duration_ms = v
        .pointer("/videoDetails/lengthSeconds")
        .and_then(|x| {
            x.as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .or_else(|| x.as_u64())
        })
        .unwrap_or(0)
        * 1000;
    Ok(VideoInfo {
        title,
        author,
        duration_ms,
        stream,
    })
}

/// Выбирает AAC-LC MP4 (itag 140) из adaptiveFormats, иначе любой AAC.
fn pick_dash_audio(v: &Value) -> Option<DashAudio> {
    let formats = v.pointer("/streamingData/adaptiveFormats")?.as_array()?;
    let mut best: Option<(bool, i64, &Value)> = None;
    for f in formats {
        let mime = f.get("mimeType").and_then(Value::as_str).unwrap_or("");
        if !mime.starts_with("audio/mp4") || f.get("url").and_then(Value::as_str).is_none() {
            continue;
        }
        let lc = mime.contains("mp4a.40.2");
        let bitrate = f.get("bitrate").and_then(Value::as_i64).unwrap_or(0);
        let better = best
            .map(|(blc, bb, _)| (lc && !blc) || (lc == blc && bitrate > bb))
            .unwrap_or(true);
        if better {
            best = Some((lc, bitrate, f));
        }
    }
    let f = best?.2;
    let url = f.get("url")?.as_str()?.to_owned();
    let len = f
        .get("contentLength")
        .and_then(|x| {
            x.as_u64()
                .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or(0);
    Some(DashAudio { url, len })
}

/// Переписывает media-URL на общий фронт. Возвращает (URL, исходный Host).
fn front_url(url: &str) -> Option<(String, String)> {
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?;
    if host != "googlevideo.com" && !host.ends_with(".googlevideo.com") {
        return None;
    }
    let mut target = format!("https://{FRONT_HOST}{}", u.path());
    if let Some(q) = u.query() {
        target.push('?');
        target.push_str(q);
    }
    Some((target, host.to_owned()))
}

/// Пробует direct и общий фронт. Манифесты сначала идут через фронт:
/// на сетях с блокировкой googlevideo он доступен, а direct висит.
async fn fetch_media(
    client: &reqwest::Client,
    url: &str,
    max: usize,
    what: &str,
) -> Result<Vec<u8>> {
    let mut targets = vec![(url.to_owned(), None)];
    // Фронт выручает только для манифестов. Медиасегменты rr*.googlevideo.com
    // через него отдают 421/404, поэтому их пробуем напрямую.
    if let Some((target, host)) = front_url(url) {
        if host == "manifest.googlevideo.com" {
            targets.insert(0, (target, Some(host)));
        }
    }
    let mut errors: Vec<String> = Vec::new();
    for (target, host) in targets {
        let label = if host.is_some() {
            format!("через {FRONT_HOST}")
        } else {
            "напрямую".to_owned()
        };
        let mut req = client.get(&target).header("User-Agent", IOS_UA);
        if let Some(host) = host {
            req = req.header(reqwest::header::HOST, host);
        }
        match tokio::time::timeout(MEDIA_TIMEOUT, req.send()).await {
            Ok(Ok(resp)) => {
                let status = resp.status();
                if status.is_success() {
                    let bytes = resp.bytes().await.with_context(|| what.to_owned())?;
                    if bytes.len() > max {
                        bail!("{what}: ответ больше {max} байт");
                    }
                    if bytes.is_empty() {
                        errors.push(format!("{label}: пустой ответ"));
                        continue;
                    }
                    return Ok(bytes.to_vec());
                }
                errors.push(format!("{label}: HTTP {status}"));
                if status.as_u16() == 403 || status.as_u16() == 410 {
                    break;
                }
            }
            Ok(Err(e)) => errors.push(format!("{label}: {e:#}")),
            Err(_) => errors.push(format!("{label}: тайм-аут")),
        }
    }
    bail!("{what}: {}", errors.join("; "))
}

async fn fetch_text(client: &reqwest::Client, url: &str, what: &str) -> Result<String> {
    let bytes = fetch_media(client, url, MAX_JSON, what).await?;
    String::from_utf8(bytes).with_context(|| format!("{what}: не UTF-8"))
}

async fn audio_segments(client: &reqwest::Client, master: &str) -> Result<Vec<Segment>> {
    let master = fetch_text(client, master, "HLS master").await?;
    let playlist = pick_audio_playlist(&master)
        .ok_or_else(|| anyhow::anyhow!("YouTube: в HLS нет аудиодорожки"))?;
    let body = fetch_text(client, &playlist, "HLS playlist").await?;
    let segments = parse_segments_detailed(&body);
    if segments.is_empty() {
        bail!("YouTube: пустой HLS-плейлист");
    }
    Ok(segments)
}

async fn fetch_segment(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    // DPI-обход иногда срабатывает не с первого раза; один повтор заметно
    // снижает вероятность «включаю» без звука.
    match fetch_media(client, url, MAX_SEGMENT, "HLS сегмент").await {
        Ok(bytes) => Ok(bytes),
        Err(first) => {
            tokio::time::sleep(Duration::from_millis(300)).await;
            fetch_media(client, url, MAX_SEGMENT, "HLS сегмент")
                .await
                .map_err(|second| anyhow::anyhow!("{first:#}; повтор: {second:#}"))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn stream_hls(
    client: reqwest::Client,
    video_id: String,
    mut segments: Vec<Segment>,
    mut index: usize,
    tx: mpsc::Sender<(u32, Vec<i16>)>,
    stop: Arc<AtomicBool>,
    _position: Arc<AtomicU64>,
) {
    let mut refreshes = 0u8;
    while index < segments.len() {
        if stop.load(Ordering::Acquire) {
            return;
        }
        match fetch_segment(&client, &segments[index].url).await {
            Ok(bytes) => match tokio::task::spawn_blocking(move || decode_adts(&bytes)).await {
                Ok(Ok((rate, mono))) => {
                    let per = (rate as usize * CHUNK_MS as usize / 1000).max(1);
                    for chunk in mono.chunks(per) {
                        if stop.load(Ordering::Acquire) {
                            return;
                        }
                        if tx.send((rate, chunk.to_vec())).await.is_err() {
                            return;
                        }
                    }
                    index += 1;
                }
                Ok(Err(e)) => {
                    tracing::warn!("youtube: сегмент {index}: {e:#}");
                    index += 1;
                }
                Err(e) => {
                    tracing::warn!("youtube: декодер: {e}");
                    return;
                }
            },
            Err(e) => {
                if refreshes >= 2 {
                    tracing::warn!("youtube: поток остановлен: {e:#}");
                    return;
                }
                refreshes += 1;
                tracing::info!("youtube: обновляю HLS после {e:#}");
                match video_info(&client, &video_id).await {
                    Ok(info) => match info.stream {
                        Stream::Hls(master) => match audio_segments(&client, &master).await {
                            Ok(fresh) if !fresh.is_empty() => segments = fresh,
                            Ok(_) => return,
                            Err(e) => {
                                tracing::warn!("youtube: HLS не обновился: {e:#}");
                                return;
                            }
                        },
                        Stream::Dash(_) => return,
                    },
                    Err(e) => {
                        tracing::warn!("youtube: видео не обновилось: {e:#}");
                        return;
                    }
                }
            }
        }
    }
    tracing::info!("youtube: воспроизведение завершено");
}

/// Громкость YouTube во время речи ассистента, проценты.
const YOUTUBE_DUCK_PERCENT: i32 = 20;

fn scaled_sample(sample: i16, percent: i32) -> i16 {
    (i32::from(sample) * percent / 100) as i16
}

fn audio_thread(
    mut rx: mpsc::Receiver<(u32, Vec<i16>)>,
    paused: Arc<AtomicBool>,
    ducked: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    position: Arc<AtomicU64>,
    pcm_name: &str,
) {
    let mut pcm = match Pcm::open(None, pcm_name, OUT_RATE, 2, OUT_PERIOD_MS, false) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("youtube: ALSA {pcm_name}: {e:#}");
            return;
        }
    };
    let mut resampler: Option<(u32, LinearResampler)> = None;
    let period = (OUT_RATE as usize * OUT_PERIOD_MS as usize / 1000).max(1);
    while let Some((rate, mono)) = rx.blocking_recv() {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let mono = if rate == OUT_RATE {
            resampler = None;
            mono
        } else {
            if resampler.as_ref().map(|(r, _)| *r) != Some(rate) {
                resampler = Some((rate, LinearResampler::new(rate, OUT_RATE)));
            }
            let (_, r) = resampler.as_mut().expect("resampler");
            r.process(&mono)
        };
        let mut flushed = false;
        for block in mono.chunks(period) {
            loop {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                if !paused.load(Ordering::Acquire) {
                    break;
                }
                if !flushed {
                    pcm.stop();
                    flushed = true;
                }
                thread::sleep(Duration::from_millis(20));
            }
            flushed = false;
            // Приглушение — это уменьшение амплитуды, а не пауза: звук
            // продолжает идти, позиция не прыгает, после ответа громкость
            // возвращается без перезапуска трека.
            let gain = if ducked.load(Ordering::Acquire) {
                YOUTUBE_DUCK_PERCENT
            } else {
                100
            };
            let mut stereo = Vec::with_capacity(block.len() * 2);
            for &s in block {
                let v = scaled_sample(s, gain);
                stereo.push(v);
                stereo.push(v);
            }
            if let Err(e) = pcm.write(&stereo) {
                tracing::warn!("youtube: ALSA: {e:#}");
                thread::sleep(Duration::from_millis(100));
            } else {
                position.fetch_add(
                    block.len() as u64 * 1000 / OUT_RATE as u64,
                    Ordering::Relaxed,
                );
            }
        }
    }
    if !stop.load(Ordering::Acquire) {
        let _ = pcm.drain();
    }
}

/// Достаёт id из ссылок youtu.be, watch?v=, shorts, live и встраиваний.
pub fn extract_video_id(text: &str) -> Option<String> {
    let text = text.trim();
    if text.len() == 11
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Some(text.to_owned());
    }
    let url = url::Url::parse(text).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    if !(host == "youtu.be"
        || host.ends_with(".youtube.com")
        || host == "youtube.com"
        || host.ends_with(".youtube-nocookie.com"))
    {
        return None;
    }
    if host == "youtu.be" {
        return url
            .path_segments()
            .and_then(|mut s| s.find(|p| !p.is_empty()))
            .and_then(clean_id);
    }
    if let Some(v) = url
        .query_pairs()
        .find(|(k, _)| k == "v")
        .map(|(_, v)| v.into_owned())
        .and_then(|v| clean_id(&v))
    {
        return Some(v);
    }
    let mut parts = url.path_segments()?;
    match parts.next()? {
        "shorts" | "live" | "embed" | "v" => parts.next().and_then(clean_id),
        _ => None,
    }
}

fn clean_id(s: &str) -> Option<String> {
    let id = s.split(['&', '?', '/']).next().unwrap_or(s);
    (id.len() == 11
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    .then(|| id.to_owned())
}

fn first_video(v: &Value) -> Option<(String, String, String)> {
    fn walk(v: &Value, depth: u8) -> Option<(String, String, String)> {
        if depth > 12 {
            return None;
        }
        if let Some(r) = v.get("videoRenderer") {
            if let Some(id) = r.get("videoId").and_then(Value::as_str) {
                if id.len() == 11
                    && id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                {
                    let title = r
                        .pointer("/title/runs/0/text")
                        .and_then(Value::as_str)
                        .or_else(|| r.pointer("/title/simpleText").and_then(Value::as_str))
                        .unwrap_or("YouTube")
                        .to_owned();
                    let author = r
                        .pointer("/ownerText/runs/0/text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    return Some((id.to_owned(), title, author));
                }
            }
        }
        match v {
            Value::Array(items) => items.iter().find_map(|x| walk(x, depth + 1)),
            Value::Object(map) => map.values().find_map(|x| walk(x, depth + 1)),
            _ => None,
        }
    }
    walk(v, 0)
}

/// Выбирает из master-плейлиста аудио с AAC 128k (itag 140/234).
fn pick_audio_playlist(master: &str) -> Option<String> {
    let mut audio = Vec::new();
    for line in master.lines() {
        if !line.starts_with("#EXT-X-MEDIA:") || !line.contains("TYPE=AUDIO") {
            continue;
        }
        let Some(uri) = line
            .split("URI=\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
        else {
            continue;
        };
        audio.push(uri.to_owned());
    }
    audio
        .iter()
        .find(|u| u.contains("itag/234") || u.contains("itag/140"))
        .or_else(|| audio.first())
        .cloned()
}

fn parse_segments_detailed(playlist: &str) -> Vec<Segment> {
    if playlist.lines().any(|l| l.starts_with("#EXT-X-MAP:")) {
        tracing::warn!("youtube: HLS с EXT-X-MAP (fMP4) не поддержан");
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut duration_ms = 0u64;
    for line in playlist.lines().map(str::trim) {
        if let Some(v) = line.strip_prefix("#EXTINF:") {
            let secs = v.split(',').next().unwrap_or("").trim();
            duration_ms = secs
                .parse::<f64>()
                .map(|s| (s * 1000.0).round() as u64)
                .unwrap_or(0);
        } else if !line.is_empty() && !line.starts_with('#') && line.starts_with("http") {
            out.push(Segment {
                url: line.to_owned(),
                duration_ms,
            });
            duration_ms = 0;
        }
    }
    out
}

/// Совместимость с диагностикой: только URL.
#[cfg(test)]
fn parse_segments(playlist: &str) -> Vec<String> {
    parse_segments_detailed(playlist)
        .into_iter()
        .map(|s| s.url)
        .collect()
}

/// Индекс сегмента, с которого начинается позиция `start_ms`.
fn segments_after(segments: &[Segment], start_ms: u64) -> usize {
    let mut acc = 0u64;
    for (i, s) in segments.iter().enumerate() {
        if acc + s.duration_ms > start_ms {
            return i;
        }
        acc += s.duration_ms;
    }
    segments.len().saturating_sub(1)
}

/// Декодирует один HLS-сегмент (ADTS AAC, возможно с ID3) в моно PCM.
pub fn decode_adts(bytes: &[u8]) -> Result<(u32, Vec<i16>)> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(bytes.to_vec())), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("aac");
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .context("AAC: контейнер")?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| anyhow::anyhow!("AAC: нет аудиодорожки"))?;
    let params = match &track.codec_params {
        Some(CodecParameters::Audio(p)) => p.clone(),
        _ => bail!("AAC: неверные параметры дорожки"),
    };
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .context("AAC: декодер")?;
    let mut rate = 0u32;
    let mut out: Vec<i16> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(e) => {
                if out.is_empty() {
                    return Err(anyhow::anyhow!("AAC: {e}"));
                }
                break;
            }
        };
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => bail!("AAC: декодирование: {e}"),
        };
        rate = decoded.spec().rate();
        let channels = decoded.spec().channels().count();
        if channels == 0 {
            continue;
        }
        let mut interleaved = vec![0i16; decoded.samples_interleaved()];
        decoded.copy_to_slice_interleaved(&mut interleaved[..]);
        if channels == 1 {
            out.extend_from_slice(&interleaved);
        } else {
            for frame in interleaved.chunks_exact(channels) {
                let sum: i32 = frame.iter().map(|&s| i32::from(s)).sum();
                out.push((sum / channels as i32) as i16);
            }
        }
    }
    if rate == 0 {
        bail!("AAC: пустой сегмент");
    }
    Ok((rate, out))
}

/// Читает MP4 по HTTP Range: DASH-аудио бывает на 30–90 МБ, целиком
/// в память его грузить нельзя. Symphonia может искать к moov/индексу.
struct HttpRangeReader {
    client: reqwest::blocking::Client,
    url: String,
    pos: u64,
    len: u64,
    buf: Vec<u8>,
    buf_pos: u64,
}

const DASH_CHUNK: u64 = 512 * 1024;

impl HttpRangeReader {
    fn new(proxy: &str, url: &str, len: u64) -> Result<Self> {
        let mut builder = reqwest::blocking::Client::builder()
            .user_agent(IOS_UA)
            .http1_only()
            .connect_timeout(Duration::from_secs(6))
            .timeout(Duration::from_secs(15));
        if !proxy.is_empty() {
            builder = builder.proxy(
                reqwest::Proxy::all(proxy).with_context(|| format!("youtube_proxy {proxy}"))?,
            );
        }
        let client = builder.build().context("HTTP-клиент DASH")?;
        let len = if len != 0 {
            len
        } else {
            let resp = client
                .get(url)
                .header(reqwest::header::RANGE, "bytes=0-0")
                .send()
                .context("DASH: размер")?;
            resp.headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.rsplit('/').next())
                .and_then(|s| s.parse().ok())
                .or_else(|| resp.content_length())
                .ok_or_else(|| anyhow::anyhow!("DASH: неизвестный размер файла"))?
        };
        Ok(Self {
            client,
            url: url.to_owned(),
            pos: 0,
            len,
            buf: Vec::new(),
            buf_pos: 0,
        })
    }

    fn fill(&mut self) -> std::io::Result<()> {
        // byedpi иногда рвёт первую попытку range-запроса: без повтора
        // DASH-поток начинается с тишины.
        let mut last = None;
        for attempt in 0..3 {
            match self.fill_once() {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last = Some(e);
                    if attempt < 2 {
                        thread::sleep(Duration::from_millis(300));
                    }
                }
            }
        }
        Err(last.unwrap_or_else(|| std::io::Error::other("DASH: range не прочитан")))
    }

    fn fill_once(&mut self) -> std::io::Result<()> {
        let end = (self.pos + DASH_CHUNK - 1).min(self.len.saturating_sub(1));
        let want = (end - self.pos + 1) as usize;
        let resp = self
            .client
            .get(&self.url)
            .header(
                reqwest::header::RANGE,
                format!("bytes={}-{}", self.pos, end),
            )
            .send()
            .map_err(io_err)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(std::io::Error::other(format!("DASH: HTTP {status}")));
        }
        if status == reqwest::StatusCode::OK && self.pos != 0 {
            return Err(std::io::Error::other("DASH: сервер не поддержал Range"));
        }
        let mut buf = Vec::with_capacity(want);
        resp.take(want as u64).read_to_end(&mut buf)?;
        if buf.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "DASH: пустой range",
            ));
        }
        self.buf = buf;
        self.buf_pos = self.pos;
        Ok(())
    }
}

fn io_err(e: reqwest::Error) -> std::io::Error {
    std::io::Error::other(e)
}

impl Read for HttpRangeReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.len {
            return Ok(0);
        }
        let off = self.pos.wrapping_sub(self.buf_pos);
        if self.buf.is_empty() || self.pos < self.buf_pos || off >= self.buf.len() as u64 {
            self.fill()?;
        }
        let off = (self.pos - self.buf_pos) as usize;
        let n = out.len().min(self.buf.len() - off);
        out[..n].copy_from_slice(&self.buf[off..off + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for HttpRangeReader {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        let target = match from {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::End(n) => i128::from(self.len) + i128::from(n),
            SeekFrom::Current(n) => i128::from(self.pos) + i128::from(n),
        };
        if target < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "DASH: seek до начала",
            ));
        }
        self.pos = (target as u64).min(self.len);
        Ok(self.pos)
    }
}

impl MediaSource for HttpRangeReader {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

fn open_dash(proxy: &str, dash: &DashAudio) -> Result<DashReader> {
    let reader = HttpRangeReader::new(proxy, &dash.url, dash.len)?;
    let mss = MediaSourceStream::new(Box::new(reader), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("m4a");
    let format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .context("DASH: контейнер")?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| anyhow::anyhow!("DASH: нет аудиодорожки"))?;
    let track_id = track.id;
    let params = match &track.codec_params {
        Some(CodecParameters::Audio(p)) => p.clone(),
        _ => bail!("DASH: неверные параметры дорожки"),
    };
    let decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .context("DASH: декодер")?;
    Ok((format, decoder, track_id))
}

/// Проверяет DASH до обещания «включаю»: открывает контейнер и декодирует
/// первый аудиопакет.
async fn probe_dash(proxy: String, dash: DashAudio) -> Result<()> {
    let pcm = tokio::task::spawn_blocking(move || {
        let (mut format, mut decoder, track_id) = open_dash(&proxy, &dash)?;
        for _ in 0..64 {
            let packet = match format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => break,
                Err(e) => bail!("DASH: {e}"),
            };
            if packet.track_id != track_id {
                continue;
            }
            match decoder.decode(&packet) {
                Ok(d) if d.frames() > 0 => return Ok(()),
                Ok(_) => continue,
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(e) => bail!("DASH: {e}"),
            }
        }
        bail!("DASH: нет аудиокадров")
    })
    .await
    .context("youtube: DASH-проба")?;
    pcm
}

/// Последовательное декодирование всего DASH-аудио в PCM-канал.
/// Блокирующая функция: вызывается из spawn_blocking.
fn notify_ready(ready: &mut Option<tokio::sync::oneshot::Sender<Result<()>>>, result: Result<()>) {
    if let Some(tx) = ready.take() {
        let _ = tx.send(result);
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_dash(
    proxy: &str,
    url: &str,
    len: u64,
    start_ms: u64,
    tx: mpsc::Sender<(u32, Vec<i16>)>,
    stop: &AtomicBool,
    position: &AtomicU64,
    mut ready: Option<tokio::sync::oneshot::Sender<Result<()>>>,
) -> Result<()> {
    let result = decode_dash_inner(proxy, url, len, start_ms, tx, stop, position, &mut ready);
    if let Some(tx) = ready.take() {
        let _ = tx.send(match &result {
            Ok(()) => Ok(()),
            Err(e) => Err(anyhow::anyhow!("{e:#}")),
        });
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn decode_dash_inner(
    proxy: &str,
    url: &str,
    len: u64,
    start_ms: u64,
    tx: mpsc::Sender<(u32, Vec<i16>)>,
    stop: &AtomicBool,
    position: &AtomicU64,
    ready: &mut Option<tokio::sync::oneshot::Sender<Result<()>>>,
) -> Result<()> {
    let dash = DashAudio {
        url: url.to_owned(),
        len,
    };
    let (mut format, mut decoder, track_id) = open_dash(proxy, &dash)?;
    if start_ms > 0 {
        use symphonia::core::formats::{SeekMode, SeekTo};
        use symphonia::core::units::Time;
        let Some(time) = Time::try_from_secs_f64(start_ms as f64 / 1000.0) else {
            bail!("DASH: неверная позиция {start_ms} мс");
        };
        match format.seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time,
                track_id: Some(track_id),
            },
        ) {
            Ok(_) => {
                decoder.reset();
                position.store(start_ms, Ordering::Release);
                tracing::info!("youtube: DASH с {start_ms} мс");
            }
            Err(e) => tracing::warn!("youtube: DASH seek: {e}; играю с начала"),
        }
    }
    let mut rate = 0u32;
    let mut pending: Vec<i16> = Vec::new();
    loop {
        if stop.load(Ordering::Acquire) {
            if rate == 0 {
                bail!("DASH: остановлено до первого пакета");
            }
            return Ok(());
        }
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(e) => {
                if rate == 0 {
                    bail!("DASH: {e}");
                }
                break;
            }
        };
        if packet.track_id != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => bail!("DASH: декодирование: {e}"),
        };
        rate = decoded.spec().rate();
        let channels = decoded.spec().channels().count();
        if channels == 0 {
            continue;
        }
        if rate == 0 {
            bail!("DASH: нулевая частота");
        }
        // Сигналим «включаю» только после реального аудиокадра.
        notify_ready(ready, Ok(()));
        let mut interleaved = vec![0i16; decoded.samples_interleaved()];
        decoded.copy_to_slice_interleaved(&mut interleaved[..]);
        if channels == 1 {
            pending.extend_from_slice(&interleaved);
        } else {
            for frame in interleaved.chunks_exact(channels) {
                let sum: i32 = frame.iter().map(|&s| i32::from(s)).sum();
                pending.push((sum / channels as i32) as i16);
            }
        }
        let per = (rate as usize * CHUNK_MS as usize / 1000).max(1);
        while pending.len() >= per {
            let chunk: Vec<i16> = pending.drain(..per).collect();
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            if tx.blocking_send((rate, chunk)).is_err() {
                return Ok(());
            }
        }
    }
    if rate == 0 {
        bail!("DASH: пустой поток");
    }
    if !pending.is_empty() && tx.blocking_send((rate, pending)).is_err() {
        return Ok(());
    }
    Ok(())
}

/// Проверка произвольного HTTPS через оба клиента и прокси.
pub async fn http_check(cfg: &ToolsConfig, url: &str) -> Result<String> {
    let p = YouTubePlayer::new(cfg, String::new())?;
    let a = match p.client.get(url).send().await {
        Ok(r) => format!("{}", r.status()),
        Err(e) => format!("ошибка: {e}"),
    };
    let b = match p.tls12.get(url).send().await {
        Ok(r) => format!("{}", r.status()),
        Err(e) => format!("ошибка: {e}"),
    };
    Ok(format!("tls1.3: {a}; tls1.2: {b}"))
}

/// Проверка без звука: разрешение, источник и декодирование первого пакета.
pub async fn check(cfg: &ToolsConfig, query: &str) -> Result<String> {
    let p = YouTubePlayer::new(cfg, String::new())?;
    let (video_id, title, author, _duration, media, client) = p.start(query).await?;
    let detail = match media {
        Media::Hls(segments) => {
            let bytes = fetch_segment(&client, &segments[0].url).await?;
            let (rate, pcm) = decode_adts(&bytes)?;
            format!(
                "HLS: сегментов: {}, декодировано: {} сэмплов @ {rate} Гц",
                segments.len(),
                pcm.len()
            )
        }
        Media::Dash(dash) => {
            probe_dash(p.proxy.clone(), dash).await?;
            "DASH: контейнер и первый аудиопакет декодированы".to_owned()
        }
    };
    let mut out = format!(
        "{video_id}: {title}{}\n{detail}",
        if author.is_empty() {
            String::new()
        } else {
            format!(" — {author}")
        }
    );
    out.push_str(&format!(
        "\nпрокси: {}",
        if cfg.youtube_proxy.trim().is_empty() {
            "нет"
        } else {
            cfg.youtube_proxy.trim()
        }
    ));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duck_scales_without_silencing() {
        assert_eq!(scaled_sample(10_000, 20), 2_000);
        assert_eq!(scaled_sample(-10_000, 20), -2_000);
        assert_eq!(scaled_sample(123, 100), 123);
        assert_ne!(scaled_sample(10_000, YOUTUBE_DUCK_PERCENT), 0);
    }

    #[test]
    fn extracts_video_ids() {
        for s in [
            "dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=10",
            "https://m.youtube.com/shorts/dQw4w9WgXcQ",
            "https://www.youtube.com/live/dQw4w9WgXcQ?si=x",
            "https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ",
        ] {
            assert_eq!(extract_video_id(s).as_deref(), Some("dQw4w9WgXcQ"), "{s}");
        }
        assert_eq!(extract_video_id("просто запрос"), None);
        assert_eq!(
            extract_video_id("https://example.com/watch?v=dQw4w9WgXcQ"),
            None
        );
    }

    #[test]
    fn fronts_only_googlevideo() {
        let (u, h) = front_url("https://rr3---sn-x.googlevideo.com/videoplayback?id=1").unwrap();
        assert_eq!(h, "rr3---sn-x.googlevideo.com");
        assert_eq!(u, "https://youtubei.googleapis.com/videoplayback?id=1");
        assert!(front_url("https://i.ytimg.com/x.jpg").is_none());
        assert!(front_url("https://www.youtube.com/watch?v=x").is_none());
    }

    #[test]
    fn picks_highest_aac_audio() {
        let master = r#"#EXTM3U
#EXT-X-MEDIA:URI="https://x/playlist?itag/233",TYPE=AUDIO,GROUP-ID="aud",NAME="low"
#EXT-X-MEDIA:URI="https://x/playlist?itag/234",TYPE=AUDIO,GROUP-ID="aud",NAME="high"
#EXT-X-MEDIA:URI="https://x/tts",TYPE=SUBTITLES,GROUP-ID="vtt"
"#;
        assert_eq!(
            pick_audio_playlist(master).as_deref(),
            Some("https://x/playlist?itag/234")
        );
    }

    #[test]
    fn parses_segments_and_rejects_fmp4() {
        let p = "#EXTM3U\n#EXTINF:5,\nhttps://a/1\n#EXTINF:5,\nhttps://a/2\n";
        assert_eq!(parse_segments(p), vec!["https://a/1", "https://a/2"]);
        assert!(parse_segments("#EXTM3U\n#EXT-X-MAP:URI=\"x\"\nhttps://a/1\n").is_empty());
    }

    #[test]
    fn reads_first_search_result() {
        let v: Value = serde_json::from_str(
            r#"{"contents":{"twoColumnSearchResultsRenderer":{"primaryContents":{"sectionListRenderer":{"contents":[
                {"itemSectionRenderer":{"contents":[
                    {"videoRenderer":{"videoId":"abcdefghijk","title":{"runs":[{"text":"Подкаст"}]},"ownerText":{"runs":[{"text":"Автор"}]}}}
                ]}}
            ]}}}}}"#,
        )
        .unwrap();
        assert_eq!(
            first_video(&v),
            Some(("abcdefghijk".into(), "Подкаст".into(), "Автор".into()))
        );
    }

    #[tokio::test]
    #[ignore = "сеть: живой YouTube"]
    async fn resolves_and_decodes_live_video() {
        let cfg = ToolsConfig::default();
        let p = YouTubePlayer::new(&cfg, "default".into()).unwrap();
        let info = video_info(&p.client, "dQw4w9WgXcQ").await.unwrap();
        let Stream::Hls(master) = info.stream else {
            panic!("у Rick Astley ожидался HLS");
        };
        let segments = audio_segments(&p.client, &master).await.unwrap();
        assert!(segments.len() > 3);
        let bytes = fetch_segment(&p.client, &segments[0].url).await.unwrap();
        let (rate, pcm) = decode_adts(&bytes).unwrap();
        assert_eq!(rate, 44_100);
        assert!(pcm.len() > 44_100);
    }

    #[tokio::test]
    #[ignore = "сеть: живой YouTube"]
    async fn probes_live_dash_podcast() {
        let cfg = ToolsConfig::default();
        let p = YouTubePlayer::new(&cfg, "default".into()).unwrap();
        // Подкаст «В два ствола»: у него нет HLS, только DASH-аудио.
        let info = video_info(&p.client, "JXzxv5AmVB4").await.unwrap();
        let Stream::Dash(dash) = info.stream else {
            panic!("у подкаста ожидался DASH");
        };
        probe_dash(p.proxy.clone(), dash).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "сеть: живой YouTube"]
    async fn decodes_live_dash_audio() {
        let cfg = ToolsConfig::default();
        let p = YouTubePlayer::new(&cfg, "default".into()).unwrap();
        let info = video_info(&p.client, "JXzxv5AmVB4").await.unwrap();
        let Stream::Dash(dash) = info.stream else {
            panic!("у подкаста ожидался DASH");
        };
        let (rate, samples) = tokio::task::spawn_blocking(move || {
            let (mut format, mut decoder, track_id) = open_dash("", &dash)?;
            let mut rate = 0u32;
            let mut samples = 0usize;
            while samples < 96_000 {
                let packet = match format.next_packet() {
                    Ok(Some(p)) => p,
                    Ok(None) => break,
                    Err(e) => bail!("DASH: {e}"),
                };
                if packet.track_id != track_id {
                    continue;
                }
                let decoded = decoder.decode(&packet)?;
                rate = decoded.spec().rate();
                samples += decoded.frames();
            }
            Ok::<_, anyhow::Error>((rate, samples))
        })
        .await
        .unwrap()
        .unwrap();
        assert!(rate > 0, "нет частоты дискретизации");
        assert!(samples >= 96_000, "декодировано только {samples} сэмплов");
    }

    #[tokio::test]
    #[ignore = "сеть: живой YouTube"]
    async fn decodes_live_dash_across_ranges() {
        let cfg = ToolsConfig::default();
        let p = YouTubePlayer::new(&cfg, "default".into()).unwrap();
        let info = video_info(&p.client, "JXzxv5AmVB4").await.unwrap();
        let Stream::Dash(dash) = info.stream else {
            panic!("у подкаста ожидался DASH");
        };
        // ~40 с AAC: больше одного 512-КиБ range-окна.
        let (rate, samples) = tokio::task::spawn_blocking(move || {
            let (mut format, mut decoder, track_id) = open_dash("", &dash)?;
            let mut rate = 0u32;
            let mut samples = 0usize;
            while samples < 44_100 * 40 {
                let packet = match format.next_packet() {
                    Ok(Some(p)) => p,
                    Ok(None) => break,
                    Err(e) => bail!("DASH: {e}"),
                };
                if packet.track_id != track_id {
                    continue;
                }
                let decoded = decoder.decode(&packet)?;
                rate = decoded.spec().rate();
                samples += decoded.frames();
            }
            Ok::<_, anyhow::Error>((rate, samples))
        })
        .await
        .unwrap()
        .unwrap();
        assert!(rate > 0);
        assert!(samples >= 44_100 * 40, "только {samples} сэмплов");
    }
}
