//! Spotify Connect внутри `petrovichd` на librespot.
//!
//! Устройство постоянно зарегистрировано в Spotify Connect, даже когда
//! музыка не играет. Звук идёт в тот же ALSA-вывод, что и TTS, через
//! собственный sink без промежуточных процессов. Поиск — через Spotify Web
//! API (токен Connect-сессии или client credentials); для сессий, вошедших
//! только access token, есть резервный поиск Tavily по ссылкам open.spotify.com.
//! librespot умеет проигрывать контекст, но не искать.

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use librespot::connect::{ConnectConfig, LoadRequest, LoadRequestOptions, Spirc};
use librespot::core::authentication::Credentials;
use librespot::core::cache::Cache;
use librespot::core::config::SessionConfig;
use librespot::core::session::Session;
use librespot::playback::audio_backend::{Open, Sink, SinkError};
use librespot::playback::config::{AudioFormat, Bitrate, PlayerConfig};
use librespot::playback::decoder::AudioPacket;
use librespot::playback::mixer::{self, Mixer, MixerConfig};
use librespot::playback::player::{Player, PlayerEvent};
use petrovich_audio::alsa::Pcm;
use petrovich_core::config::{Secrets, SpotifyConfig};
use petrovich_core::ports::MusicControl;

use crate::music::DeferredMusic;

/// librespot создаёт sink через свободную функцию без захвата, поэтому
/// имя ALSA-устройства хранится в глобальной ячейке, установленной при
/// старте. Она пишется один раз до создания плеера.
static SPOTIFY_PCM: OnceLock<String> = OnceLock::new();

/// Насколько тише играет музыка, пока ассистент слушает или говорит.
/// Полный ноль раньше оставлял пользователя в тишине и навсегда сбивал
/// громкость при вложенных приглушениях; 20% слышно, но не мешает STT.
const DUCK_PERCENT: u32 = 20;

fn ducked_volume(saved: u16) -> u16 {
    if saved == 0 {
        return 0;
    }
    ((u32::from(saved) * DUCK_PERCENT / 100) as u16).max(1)
}

struct PetrovichSink {
    pcm: Option<Pcm>,
}

impl Open for PetrovichSink {
    fn open(_device: Option<String>, _format: AudioFormat) -> Self {
        let name = SPOTIFY_PCM.get().map(String::as_str).unwrap_or("default");
        let pcm = Pcm::open(None, name, 44_100, 2, 20, false)
            .map_err(|e| tracing::warn!("spotify: ALSA {name}: {e:#}"))
            .ok();
        Self { pcm }
    }
}

impl Sink for PetrovichSink {
    fn start(&mut self) -> std::result::Result<(), SinkError> {
        Ok(())
    }

    fn stop(&mut self) -> std::result::Result<(), SinkError> {
        if let Some(pcm) = self.pcm.as_mut() {
            pcm.stop();
        }
        Ok(())
    }

    fn write(
        &mut self,
        packet: AudioPacket,
        converter: &mut librespot::playback::convert::Converter,
    ) -> std::result::Result<(), SinkError> {
        let Some(pcm) = self.pcm.as_mut() else {
            return Err(SinkError::NotConnected("ALSA".into()));
        };
        match packet {
            AudioPacket::Samples(samples) => {
                let pcm16 = converter.f64_to_s16(&samples);
                pcm.write(&pcm16)
                    .map_err(|e| SinkError::OnWrite(format!("{e:#}")))
            }
            AudioPacket::Raw(bytes) => {
                // Raw приходит только с passthrough, который мы не включаем.
                let _ = bytes;
                Err(SinkError::InvalidParams("raw PCM не поддерживается".into()))
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
struct PlaybackState {
    uri: String,
    title: String,
    position_ms: u64,
    duration_ms: u64,
    playing: bool,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct SavedSession {
    uri: String,
    title: String,
    position_ms: u64,
}

#[derive(Clone, Debug)]
struct Item {
    uri: String,
    name: String,
    subtitle: String,
    kind: &'static str,
    /// Для выпусков подкастов: дата публикации, чтобы выбрать последний.
    release: String,
}

struct Api {
    client: reqwest::Client,
    session: librespot::core::Session,
    client_id: String,
    client_secret: String,
    market: String,
    limit: usize,
    token: Mutex<Option<(String, Instant)>>,
    tavily_key: String,
}

impl Api {
    fn new(
        session: librespot::core::Session,
        cfg: &SpotifyConfig,
        secret: String,
        tavily_key: String,
    ) -> Result<Self> {
        Ok(Self {
            client: {
                let mut b = reqwest::Client::builder().timeout(Duration::from_secs(12));
                if !cfg.proxy.trim().is_empty() {
                    b = b.proxy(
                        reqwest::Proxy::all(cfg.proxy.trim())
                            .with_context(|| format!("Spotify API: прокси {}", cfg.proxy))?,
                    );
                }
                b.build().context("Spotify API: клиент")?
            },
            session,
            client_id: cfg.client_id.clone(),
            client_secret: secret,
            market: cfg.market.clone(),
            limit: cfg.search_limit.clamp(1, 20),
            token: Mutex::new(None),
            tavily_key,
        })
    }

    /// Сначала токен уже авторизованной Connect-сессии: отдельный
    /// client_id не нужен, поиск идёт с теми же правами. Если сессия
    /// авторизована только access token и keymaster недоступен, работает
    /// резервный client_credentials.
    async fn token(&self) -> Result<String> {
        if let Ok(token) = self.session.token_provider().get_token("").await {
            return Ok(token.access_token);
        }
        {
            let guard = self.token.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((token, until)) = guard.as_ref() {
                if *until > Instant::now() {
                    return Ok(token.clone());
                }
            }
        }
        if self.client_id.trim().is_empty() || self.client_secret.is_empty() {
            bail!(
                "Spotify: поиск недоступен. Вход по старому токену не даёт Web API токен; \
                 добавьте spotify.client_id и client_secret в secrets.env \
                 (Spotify Developer Dashboard → Create app)."
            );
        }
        let resp = self
            .client
            .post("https://accounts.spotify.com/api/token")
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .form(&[("grant_type", "client_credentials")])
            .send()
            .await
            .context("Spotify: токен")?;
        if !resp.status().is_success() {
            bail!("Spotify: токен HTTP {}", resp.status());
        }
        let v: serde_json::Value = resp.json().await.context("Spotify: токен JSON")?;
        let token = v
            .get("access_token")
            .and_then(|x| x.as_str())
            .context("Spotify: нет access_token")?
            .to_owned();
        let ttl = v
            .get("expires_in")
            .and_then(|x| x.as_u64())
            .unwrap_or(3600)
            .saturating_sub(60);
        *self.token.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((token.clone(), Instant::now() + Duration::from_secs(ttl)));
        Ok(token)
    }

    async fn search(&self, query: &str, want_episode: bool) -> Result<Vec<Item>> {
        match self.search_api(query, want_episode).await {
            Ok(items) if !items.is_empty() => return Ok(items),
            Ok(_) => {}
            Err(e) if self.tavily_key.is_empty() => return Err(e),
            Err(e) => tracing::warn!("Spotify Web API: {e:#}; ищу через Tavily"),
        }
        self.search_tavily(query).await
    }

    async fn search_api(&self, query: &str, want_episode: bool) -> Result<Vec<Item>> {
        let token = self.token().await?;
        let types = if want_episode {
            "episode,show,track,album,playlist"
        } else {
            "track,album,playlist,episode,show"
        };
        let resp = self
            .client
            .get("https://api.spotify.com/v1/search")
            .bearer_auth(token)
            .query(&[
                ("q", query),
                ("type", types),
                ("limit", &self.limit.to_string()),
                ("market", &self.market),
            ])
            .send()
            .await
            .context("Spotify: поиск")?;
        if !resp.status().is_success() {
            bail!("Spotify: поиск HTTP {}", resp.status());
        }
        let v: serde_json::Value = resp.json().await.context("Spotify: поиск JSON")?;
        let mut items = Vec::new();
        // Отдельные блоки, чтобы не путать типы в ответе.
        if let Some(arr) = v.pointer("/tracks/items").and_then(|x| x.as_array()) {
            for it in arr {
                if let Some(item) = parse_item(it, "трек", "artists", false) {
                    items.push(item);
                }
            }
        }
        if let Some(arr) = v.pointer("/episodes/items").and_then(|x| x.as_array()) {
            for it in arr {
                if let Some(item) = parse_item(it, "выпуск", "show", true) {
                    items.push(item);
                }
            }
        }
        if let Some(arr) = v.pointer("/shows/items").and_then(|x| x.as_array()) {
            for it in arr {
                if let Some(item) = parse_item(it, "подкаст", "publisher", false) {
                    items.push(item);
                }
            }
        }
        if let Some(arr) = v.pointer("/albums/items").and_then(|x| x.as_array()) {
            for it in arr {
                if let Some(item) = parse_item(it, "альбом", "artists", false) {
                    items.push(item);
                }
            }
        }
        if let Some(arr) = v.pointer("/playlists/items").and_then(|x| x.as_array()) {
            for it in arr {
                if let Some(item) = parse_item(it, "плейлист", "owner", false) {
                    items.push(item);
                }
            }
        }
        Ok(items)
    }

    /// Резервный поиск, если вход по старому токену не даёт Web API-токен.
    /// Tavily отдаёт ссылки open.spotify.com, из которых мы берём URI.
    async fn search_tavily(&self, query: &str) -> Result<Vec<Item>> {
        if self.tavily_key.is_empty() {
            bail!("Spotify: нет ключа Tavily для резервного поиска");
        }
        let body = serde_json::json!({
            "query": format!("site:open.spotify.com {query}"),
            "search_depth": "basic",
            "max_results": self.limit.max(10),
            "include_answer": false,
            "include_raw_content": false,
        });
        let resp = self
            .client
            .post("https://api.tavily.com/search")
            .bearer_auth(&self.tavily_key)
            .json(&body)
            .send()
            .await
            .context("Spotify: Tavily")?;
        if !resp.status().is_success() {
            bail!("Spotify: Tavily HTTP {}", resp.status());
        }
        let v: serde_json::Value = resp.json().await.context("Spotify: Tavily JSON")?;
        let mut items = Vec::new();
        if let Some(arr) = v.get("results").and_then(|x| x.as_array()) {
            for r in arr {
                let Some(url) = r.get("url").and_then(|x| x.as_str()) else {
                    continue;
                };
                let Some(item) =
                    item_from_web_url(url, r.get("title").and_then(|x| x.as_str()).unwrap_or(""))
                else {
                    continue;
                };
                if !items.iter().any(|i: &Item| i.uri == item.uri) {
                    items.push(item);
                }
            }
        }
        Ok(items)
    }
}

fn item_from_web_url(url: &str, raw_title: &str) -> Option<Item> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.host_str() != Some("open.spotify.com") {
        return None;
    }
    let mut segments = parsed.path_segments()?.filter(|s| !s.is_empty());
    let mut kind = None;
    let mut id = None;
    for segment in segments.by_ref() {
        let candidate = match segment {
            "track" => Some("трек"),
            "episode" => Some("выпуск"),
            "album" => Some("альбом"),
            "playlist" => Some("плейлист"),
            "show" => Some("подкаст"),
            _ => None,
        };
        if let Some(candidate) = candidate {
            kind = Some(candidate);
            id = segments.next();
            break;
        }
    }
    let kind = kind?;
    let id = id?;
    if id.len() < 10 || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    let name = clean_web_title(raw_title);
    if name.is_empty() {
        return None;
    }
    let subtitle = raw_title
        .split(" by ")
        .nth(1)
        .map(|s| s.split(" | ").next().unwrap_or(s).trim().to_owned())
        .unwrap_or_default();
    Some(Item {
        uri: format!(
            "spotify:{kind_id}:{id}",
            kind_id = match kind {
                "трек" => "track",
                "выпуск" => "episode",
                "альбом" => "album",
                "плейлист" => "playlist",
                _ => "show",
            }
        ),
        name,
        subtitle,
        kind,
        release: String::new(),
    })
}

fn clean_web_title(raw: &str) -> String {
    let mut s = raw.trim();
    if let Some((head, _)) = s.split_once(" | Spotify") {
        s = head;
    }
    for marker in [
        " - song and lyrics by ",
        " - Song, Lyrics, and Music Video by ",
    ] {
        if let Some((head, _)) = s.split_once(marker) {
            s = head;
        }
    }
    s = s.trim_start_matches("Listen to ").trim();
    s = s.strip_suffix(" Songs").unwrap_or(s);
    s.trim().to_owned()
}

fn parse_item(
    it: &serde_json::Value,
    kind: &'static str,
    owner: &str,
    episode: bool,
) -> Option<Item> {
    let uri = it.get("uri")?.as_str()?.to_owned();
    let name = it
        .get("name")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_owned();
    if name.is_empty() {
        return None;
    }
    let subtitle = if owner == "artists" {
        it.get("artists")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.get("name").and_then(|n| n.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    } else if owner == "show" {
        it.pointer("/show/name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_owned()
    } else if owner == "publisher" {
        it.get("publisher")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_owned()
    } else {
        it.pointer("/owner/display_name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_owned()
    };
    let release = it
        .get("release_date")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_owned();
    let _ = episode;
    Some(Item {
        uri,
        name,
        subtitle,
        kind,
        release,
    })
}

fn choose(items: &[Item], want_episode: bool) -> Option<&Item> {
    if want_episode {
        // «Последний выпуск»: сортируем по дате публикации, свежие первыми.
        // Tavily-ссылки даты не содержат; тогда сохраняем порядок выдачи,
        // а не берём случайный последний элемент.
        let episodes: Vec<&Item> = items.iter().filter(|i| i.kind == "выпуск").collect();
        if let Some(ep) = episodes
            .iter()
            .copied()
            .filter(|i| !i.release.is_empty())
            .max_by(|a, b| a.release.cmp(&b.release))
        {
            return Some(ep);
        }
        if let Some(ep) = episodes.first() {
            return Some(ep);
        }
    }
    items
        .iter()
        .find(|i| i.kind == "трек")
        .or_else(|| items.iter().find(|i| i.kind == "выпуск"))
        .or_else(|| items.iter().find(|i| i.kind == "альбом"))
        .or_else(|| items.iter().find(|i| i.kind == "плейлист"))
        .or_else(|| items.first())
}

/// Специальный контекст Spotify Connect для коллекции «Мне нравится».
/// У неё нет id плейлиста — только URI с каноническим именем пользователя.
fn liked_item(username: &str) -> Option<Item> {
    if username.is_empty() {
        return None;
    }
    Some(Item {
        uri: format!("spotify:user:{username}:collection"),
        name: "Любимые треки".into(),
        subtitle: "Мне нравится".into(),
        kind: "плейлист",
        release: String::new(),
    })
}

pub struct Spotify {
    spirc: Arc<Spirc>,
    state: Arc<Mutex<PlaybackState>>,
    api: Api,
    mixer: Arc<dyn Mixer>,
    /// Канонический пользователь сессии: нужен для `spotify:user:<id>:collection`.
    username: String,
    queue: Mutex<Vec<Item>>,
    saved_volume: AtomicU16,
    /// Глубина приглушения: демон глушит музыку на время слушания,
    /// ход диалога — на время речи. Возвращаем громкость только на нуле.
    duck_depth: Arc<Mutex<u32>>,
    saved: Arc<Mutex<Option<SavedSession>>>,
    saved_path: Option<std::path::PathBuf>,
}

impl Spotify {
    /// Запускает Spotify Connect. `Ok(None)` — модуль выключен или нет
    /// учётных данных; в этом случае ничего не инициализируется.
    pub async fn start(
        cfg: &SpotifyConfig,
        secrets: &Secrets,
        playback_pcm: String,
        saved_path: Option<std::path::PathBuf>,
        tavily_key: String,
    ) -> Result<Option<Arc<Self>>> {
        let token = secrets.get(&cfg.access_token_env).unwrap_or("");
        let password = secrets.get(&cfg.password_env).unwrap_or("");
        if SPOTIFY_PCM.get().is_none() {
            let _ = SPOTIFY_PCM.set(playback_pcm);
        }
        let cache_dir = cfg.cache_dir.clone().unwrap_or_default();
        // /data на колонке — около 90 МБ. 64 МБ кэша вместе с двумя
        // релизами и резервными копиями занимали весь раздел; 16 МБ
        // хватает на несколько минут вперёд, остальное evict-ится.
        let cache = Cache::new(
            Some(cache_dir.as_path()),
            Some(cache_dir.as_path()),
            Some(cache_dir.join("files").as_path()),
            Some(16 * 1024 * 1024),
        )
        .context("Spotify: кэш")?;
        let cached = cache.credentials();
        if cached.is_none() && token.is_empty() && (cfg.username.is_empty() || password.is_empty())
        {
            tracing::warn!("Spotify включён, но нет access token, логина/пароля или кэша");
            return Ok(None);
        }
        let credentials = cached
            .or_else(|| {
                if !token.is_empty() {
                    Some(Credentials::with_access_token(token))
                } else {
                    Some(Credentials::with_password(&cfg.username, password))
                }
            })
            .context("Spotify: нет учётных данных")?;
        let mut session_cfg = SessionConfig {
            autoplay: Some(false),
            ..SessionConfig::default()
        };
        if !cfg.proxy.trim().is_empty() {
            match url::Url::parse(cfg.proxy.trim()) {
                Ok(url) if matches!(url.scheme(), "http" | "https") => {
                    session_cfg.proxy = Some(url);
                }
                Ok(_) => tracing::warn!(
                    "Spotify: librespot поддерживает только HTTP(S)-прокси; {} пропущен",
                    cfg.proxy
                ),
                Err(e) => tracing::warn!("Spotify: прокси {}: {e}", cfg.proxy),
            }
        }
        let session = Session::new(session_cfg, Some(cache));
        let mixer = mixer::find(None).context("Spotify: микшер")?(MixerConfig::default())
            .context("Spotify: микшер")?;
        let bitrate = match cfg.bitrate {
            96 => Bitrate::Bitrate96,
            320 => Bitrate::Bitrate320,
            _ => Bitrate::Bitrate160,
        };
        let player_cfg = PlayerConfig {
            bitrate,
            position_update_interval: Some(Duration::from_secs(1)),
            ..PlayerConfig::default()
        };
        let player = Player::new(player_cfg, session.clone(), mixer.get_soft_volume(), || {
            Box::new(PetrovichSink::open(None, AudioFormat::S16))
        });
        // `ConnectConfig` по умолчанию стартует с 50% (`u16::MAX / 2`), а TTS
        // идёт в ALSA напрямую. Из-за этого музыка звучала заметно тише речи.
        // Громкость колонки должен задавать только ALSA, поэтому softmixer
        // стартует на полной шкале.
        let connect_cfg = ConnectConfig {
            name: cfg.device_name.clone(),
            initial_volume: u16::MAX,
            ..ConnectConfig::default()
        };
        let api_session = session.clone();
        let (spirc, task) = Spirc::new(
            connect_cfg,
            session,
            credentials,
            player.clone(),
            mixer.clone(),
        )
        .await
        .context("Spotify: Connect")?;
        tokio::spawn(task);
        let spirc = Arc::new(spirc);
        let state = Arc::new(Mutex::new(PlaybackState::default()));
        let username = api_session.username();
        let api = Api::new(
            api_session,
            cfg,
            secrets
                .get(&cfg.client_secret_env)
                .unwrap_or_default()
                .to_owned(),
            tavily_key,
        )?;
        let saved = saved_path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok());
        let app = Arc::new(Self {
            spirc: spirc.clone(),
            state: state.clone(),
            api,
            mixer: mixer.clone(),
            username,
            queue: Mutex::new(Vec::new()),
            saved_volume: AtomicU16::new(mixer.volume()),
            duck_depth: Arc::new(Mutex::new(0)),
            saved: Arc::new(Mutex::new(saved)),
            saved_path,
        });
        // События плеера: название, позиция, длительность.
        let mut events = player.get_player_event_channel();
        let state_events = state.clone();
        let saved_events = app.saved.clone();
        let saved_path_events = app.saved_path.clone();
        tokio::spawn(async move {
            let mut last_saved = Instant::now();
            while let Some(ev) = events.recv().await {
                let mut st = state_events.lock().unwrap_or_else(|e| e.into_inner());
                let mut persist = false;
                match ev {
                    PlayerEvent::TrackChanged { audio_item } => {
                        st.title = audio_item.name.clone();
                        st.uri = audio_item.uri.clone();
                        st.duration_ms = u64::from(audio_item.duration_ms);
                        st.position_ms = 0;
                        persist = true;
                    }
                    PlayerEvent::Playing { position_ms, .. }
                    | PlayerEvent::PositionChanged { position_ms, .. }
                    | PlayerEvent::PositionCorrection { position_ms, .. }
                    | PlayerEvent::Seeked { position_ms, .. } => {
                        st.position_ms = u64::from(position_ms);
                        st.playing = true;
                    }
                    PlayerEvent::Paused { position_ms, .. } => {
                        st.position_ms = u64::from(position_ms);
                        st.playing = false;
                        persist = true;
                    }
                    PlayerEvent::Stopped { .. } | PlayerEvent::EndOfTrack { .. } => {
                        st.playing = false;
                    }
                    _ => {}
                }
                let snapshot = st.clone();
                drop(st);
                if persist || (snapshot.playing && last_saved.elapsed() >= Duration::from_secs(15))
                {
                    save_snapshot(&snapshot, &saved_events, &saved_path_events);
                    last_saved = Instant::now();
                }
                // Экран обновляет демон раз в секунду (`tick`): иначе события
                // Spotify перебивают позицию YouTube и цифры прыгают.
            }
        });
        tracing::info!("Spotify Connect: «{}» готов", cfg.device_name);
        Ok(Some(app))
    }

    fn save_session(&self) {
        let st = self.state();
        save_snapshot(&st, &self.saved, &self.saved_path);
    }

    fn load_uri(&self, item: &Item, start_ms: u32) -> Result<()> {
        let opts = LoadRequestOptions {
            start_playing: true,
            seek_to: start_ms,
            ..LoadRequestOptions::default()
        };
        let req = if matches!(item.kind, "трек" | "выпуск") {
            LoadRequest::from_tracks(vec![item.uri.clone()], opts)
        } else {
            LoadRequest::from_context_uri(item.uri.clone(), opts)
        };
        // librespot молча игнорирует Load и Play, пока устройство не активно
        // (после перезапуска демона, простоя или передачи на телефон).
        // Активация идёт в той же очереди команд, поэтому Load выполнится
        // уже на активном устройстве.
        self.clear_duck();
        self.spirc.activate()?;
        self.spirc.load(req)?;
        self.spirc.play()?;
        Ok(())
    }

    async fn search_items(&self, query: &str) -> Result<Vec<Item>> {
        let want_episode =
            query.to_lowercase().contains("выпуск") || query.to_lowercase().contains("подкаст");
        self.api.search(query, want_episode).await
    }

    /// Ждёт реального события от плеера, а не только успешной отправки
    /// команды Spirc. Иначе «Включаю» звучит при проигнорированной команде.
    async fn wait_started(&self, before: &PlaybackState) -> Result<()> {
        for _ in 0..50 {
            let st = self.state();
            if st.uri != before.uri || (st.playing && !before.playing) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        bail!("Spotify: команда принята, но воспроизведение не началось")
    }

    fn state(&self) -> PlaybackState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Снять приглушение целиком: запуск музыки сам по себе означает, что её
    /// должно быть слышно, кто бы её ни приглушил до этого.
    fn clear_duck(&self) {
        let mut depth = self.duck_depth.lock().unwrap_or_else(|e| e.into_inner());
        if *depth == 0 {
            return;
        }
        *depth = 0;
        let v = self.saved_volume.load(Ordering::Relaxed);
        self.mixer.set_volume(v);
    }
}

#[async_trait::async_trait]
impl MusicControl for Spotify {
    fn play(&self) -> petrovich_core::Result<()> {
        let st = self.state();
        if st.uri.is_empty() {
            return Err(core_err(anyhow::anyhow!(
                "Spotify: нет выбранного трека. Скажите название, исполнителя или «включи сохранённое»."
            )));
        }
        self.clear_duck();
        self.spirc.activate().map_err(|e| core_err(e.into()))?;
        self.spirc.play().map_err(|e| core_err(e.into()))
    }

    async fn resume_checked(&self) -> petrovich_core::Result<()> {
        let before = self.state();
        if before.uri.is_empty() {
            return Err(core_err(anyhow::anyhow!(
                "Spotify: нечего продолжать. Скажите название или «включи сохранённое»."
            )));
        }
        if before.playing {
            return Ok(());
        }
        self.clear_duck();
        self.spirc.activate().map_err(|e| core_err(e.into()))?;
        self.spirc.play().map_err(|e| core_err(e.into()))?;
        self.wait_started(&before).await.map_err(core_err)
    }

    async fn play_search(&self, query: &str) -> petrovich_core::Result<String> {
        let items = self.search_items(query).await.map_err(core_err)?;
        let want_episode = query.to_lowercase().contains("выпуск");
        let item = choose(&items, want_episode)
            .cloned()
            .ok_or_else(|| core_err(anyhow::anyhow!("Spotify: ничего не найдено")))?;
        let before = self.state();
        self.load_uri(&item, 0).map_err(core_err)?;
        self.wait_started(&before).await.map_err(core_err)?;
        let mut text = format!("Включаю: {} — {}", item.name, item.subtitle);
        if item.kind != "трек" {
            text.push_str(&format!(" ({})", item.kind));
        }
        Ok(text)
    }

    async fn liked(&self) -> petrovich_core::Result<String> {
        let item = liked_item(&self.username).ok_or_else(|| {
            core_err(anyhow::anyhow!(
                "Spotify: не знаю пользователя, коллекция «Мне нравится» недоступна"
            ))
        })?;
        let before = self.state();
        self.load_uri(&item, 0).map_err(core_err)?;
        self.wait_started(&before).await.map_err(core_err)?;
        Ok("Включаю любимые треки из «Мне нравится».".into())
    }

    fn pause(&self) -> petrovich_core::Result<()> {
        self.spirc.pause().map_err(|e| core_err(e.into()))?;
        self.save_session();
        Ok(())
    }

    fn resume(&self) -> petrovich_core::Result<()> {
        self.play()
    }

    fn next(&self) -> petrovich_core::Result<()> {
        let next = self
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .first()
            .cloned();
        if let Some(item) = next {
            if let Ok(mut q) = self.queue.lock() {
                if !q.is_empty() {
                    q.remove(0);
                }
            }
            self.load_uri(&item, 0).map_err(core_err)?;
        } else {
            self.spirc.next().map_err(|e| core_err(e.into()))?;
        }
        Ok(())
    }

    fn previous(&self) -> petrovich_core::Result<()> {
        self.spirc.prev().map_err(|e| core_err(e.into()))
    }

    fn status(&self) -> petrovich_core::Result<String> {
        let st = self.state();
        if st.title.is_empty() {
            return Ok("Spotify: устройство готово, ничего не играет.".into());
        }
        let pos = st.position_ms / 1000;
        let dur = st.duration_ms / 1000;
        Ok(format!(
            "{}Spotify: {}{}",
            if st.playing { "" } else { "Пауза. " },
            st.title,
            if dur > 0 {
                format!(", {pos}:{:02} из {}:{:02}", pos % 60, dur / 60, dur % 60)
            } else {
                String::new()
            }
        ))
    }

    fn is_playing(&self) -> petrovich_core::Result<bool> {
        Ok(self.state().playing)
    }

    fn seek_by(&self, delta_ms: i64) -> petrovich_core::Result<()> {
        let cur = self.state().position_ms as i64;
        let target = (cur + delta_ms).max(0) as u32;
        self.spirc
            .set_position_ms(target)
            .map_err(|e| core_err(e.into()))?;
        if let Ok(mut st) = self.state.lock() {
            st.position_ms = u64::from(target);
        }
        self.save_session();
        Ok(())
    }

    fn seek_to(&self, position_ms: u64) -> petrovich_core::Result<()> {
        let target = position_ms.min(u64::from(u32::MAX)) as u32;
        self.spirc
            .set_position_ms(target)
            .map_err(|e| core_err(e.into()))?;
        if let Ok(mut st) = self.state.lock() {
            st.position_ms = u64::from(target);
        }
        self.save_session();
        Ok(())
    }

    async fn queue(&self, query: &str) -> petrovich_core::Result<String> {
        let items = self.search_items(query).await.map_err(core_err)?;
        let want_episode = query.to_lowercase().contains("выпуск");
        let item = choose(&items, want_episode)
            .cloned()
            .ok_or_else(|| core_err(anyhow::anyhow!("Spotify: ничего не найдено")))?;
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        q.push(item.clone());
        Ok(format!("В очереди: {} — {}.", item.name, item.subtitle))
    }

    async fn resume_saved(&self) -> petrovich_core::Result<String> {
        let saved = self
            .saved_path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<SavedSession>(&b).ok())
            .or_else(|| self.saved.lock().unwrap_or_else(|e| e.into_inner()).clone())
            .filter(|s| !s.uri.is_empty())
            .ok_or_else(|| core_err(anyhow::anyhow!("нет сохранённой позиции Spotify")))?;
        let kind = if saved.uri.contains(":episode:") {
            "выпуск"
        } else if saved.uri.contains(":track:") {
            "трек"
        } else if saved.uri.contains(":album:") {
            "альбом"
        } else if saved.uri.contains(":playlist:") {
            "плейлист"
        } else {
            "подкаст"
        };
        let item = Item {
            uri: saved.uri.clone(),
            name: saved.title.clone(),
            subtitle: String::new(),
            kind,
            release: String::new(),
        };
        let before = self.state();
        self.load_uri(&item, saved.position_ms.min(u64::from(u32::MAX)) as u32)
            .map_err(core_err)?;
        self.wait_started(&before).await.map_err(core_err)?;
        Ok(format!(
            "Продолжаю: {} с {}:{:02}.",
            saved.title,
            saved.position_ms / 60_000,
            (saved.position_ms / 1000) % 60
        ))
    }

    fn sleep_timer(&self, minutes: u64) -> petrovich_core::Result<()> {
        let spirc = self.spirc.clone();
        let minutes = minutes.clamp(1, 24 * 60);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(minutes * 60)).await;
            tracing::info!("spotify: таймер сна на {minutes} мин");
            let _ = spirc.pause();
        });
        Ok(())
    }

    fn duck(&self) {
        let mut depth = self.duck_depth.lock().unwrap_or_else(|e| e.into_inner());
        if *depth == 0 {
            let saved = self.mixer.volume();
            self.saved_volume.store(saved, Ordering::Relaxed);
            self.mixer.set_volume(ducked_volume(saved));
        }
        *depth += 1;
    }

    fn unduck(&self) {
        let mut depth = self.duck_depth.lock().unwrap_or_else(|e| e.into_inner());
        if *depth == 0 {
            return;
        }
        *depth -= 1;
        if *depth == 0 {
            let v = self.saved_volume.load(Ordering::Relaxed);
            self.mixer.set_volume(v);
        }
    }

    fn position(&self) -> petrovich_core::Result<Option<(u64, u64)>> {
        let st = self.state();
        if st.title.is_empty() {
            return Ok(None);
        }
        Ok(Some((st.position_ms / 1000, st.duration_ms / 1000)))
    }
}

/// Запускает Spotify и, если первая попытка не удалась, повторяет её в фоне.
///
/// На холодном старте демон может опередить hysteria2: SOCKS5 на 1080 ещё не
/// слушает, librespot получает `unexpected EOF while tunnel reading`, и без
/// повтора Spotify остаётся выключенным до следующего перезапуска демона.
pub async fn start_resilient(
    cfg: SpotifyConfig,
    secrets: Secrets,
    playback_pcm: String,
    saved_path: Option<std::path::PathBuf>,
    tavily_key: String,
) -> Option<Arc<dyn MusicControl>> {
    match Spotify::start(
        &cfg,
        &secrets,
        playback_pcm.clone(),
        saved_path.clone(),
        tavily_key.clone(),
    )
    .await
    {
        Ok(Some(spotify)) => {
            tracing::info!("Spotify Connect: «{}»", cfg.device_name);
            return Some(spotify);
        }
        Ok(None) => return None,
        Err(e) => tracing::warn!("Spotify недоступен: {e:#}; повторяю в фоне"),
    }
    let slot = DeferredMusic::new(
        "Spotify: подключение ещё не готово, прокси не отвечает. Повторите через полминуты.",
    );
    let retry = slot.clone();
    let name = cfg.device_name.clone();
    tokio::spawn(async move {
        let mut delay = Duration::from_secs(5);
        loop {
            tokio::time::sleep(delay).await;
            match Spotify::start(
                &cfg,
                &secrets,
                playback_pcm.clone(),
                saved_path.clone(),
                tavily_key.clone(),
            )
            .await
            {
                Ok(Some(spotify)) => {
                    retry.install(spotify);
                    tracing::info!("Spotify Connect: «{name}» подключён после повтора");
                    return;
                }
                Ok(None) => {
                    tracing::warn!("Spotify: учётные данные пропали, повтор остановлен");
                    return;
                }
                Err(e) => {
                    delay = (delay * 2).min(Duration::from_secs(60));
                    tracing::warn!(
                        "Spotify: повтор не удался: {e:#}; следующий через {} с",
                        delay.as_secs()
                    );
                }
            }
        }
    });
    Some(slot)
}

fn save_snapshot(
    st: &PlaybackState,
    saved: &Mutex<Option<SavedSession>>,
    path: &Option<std::path::PathBuf>,
) {
    if st.uri.is_empty() {
        return;
    }
    let session = SavedSession {
        uri: st.uri.clone(),
        title: st.title.clone(),
        position_ms: st.position_ms,
    };
    *saved.lock().unwrap_or_else(|e| e.into_inner()) = Some(session.clone());
    let Some(path) = path else { return };
    let Ok(bytes) = serde_json::to_vec(&session) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn core_err(e: anyhow::Error) -> petrovich_core::Error {
    petrovich_core::Error::External(format!("Spotify: {e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duck_keeps_music_audible_instead_of_muting_it() {
        assert_eq!(ducked_volume(0), 0);
        assert_eq!(ducked_volume(10), 2);
        assert_eq!(ducked_volume(50), 10);
        assert_eq!(ducked_volume(u16::MAX), u16::MAX / 5);
    }

    #[test]
    fn builds_liked_collection_uri() {
        let item = liked_item("31hensidfy3hxflkgpjduzuztiey").unwrap();
        assert_eq!(
            item.uri,
            "spotify:user:31hensidfy3hxflkgpjduzuztiey:collection"
        );
        assert_eq!(item.kind, "плейлист");
        assert!(liked_item("").is_none());
    }

    #[test]
    fn picks_latest_episode_for_podcast_query() {
        let items = vec![
            Item {
                uri: "spotify:episode:old".into(),
                name: "Старый выпуск".into(),
                subtitle: "Подкаст X".into(),
                kind: "выпуск",
                release: "2025-01-01".into(),
            },
            Item {
                uri: "spotify:episode:new".into(),
                name: "Новый выпуск".into(),
                subtitle: "Подкаст X".into(),
                kind: "выпуск",
                release: "2026-02-01".into(),
            },
            Item {
                uri: "spotify:track:noise".into(),
                name: "Похожий трек".into(),
                subtitle: "Noise".into(),
                kind: "трек",
                release: String::new(),
            },
        ];
        let chosen = choose(&items, true).unwrap();
        assert_eq!(chosen.uri, "spotify:episode:new");
        let chosen = choose(&items, false).unwrap();
        assert_eq!(chosen.uri, "spotify:track:noise");
    }

    #[test]
    fn keeps_search_order_for_undated_episodes() {
        let items = vec![
            Item {
                uri: "spotify:episode:first".into(),
                name: "Первый в выдаче".into(),
                subtitle: String::new(),
                kind: "выпуск",
                release: String::new(),
            },
            Item {
                uri: "spotify:episode:second".into(),
                name: "Второй в выдаче".into(),
                subtitle: String::new(),
                kind: "выпуск",
                release: String::new(),
            },
        ];
        assert_eq!(choose(&items, true).unwrap().uri, "spotify:episode:first");
    }

    #[test]
    fn saves_and_loads_position() {
        let dir = std::env::temp_dir().join(format!("petrovich-spotify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("spotify.json");
        let st = PlaybackState {
            uri: "spotify:episode:42".into(),
            title: "Выпуск".into(),
            position_ms: 123_000,
            duration_ms: 600_000,
            playing: true,
        };
        let saved = Mutex::new(None);
        save_snapshot(&st, &saved, &Some(path.clone()));
        let bytes = std::fs::read(&path).unwrap();
        let loaded: SavedSession = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(loaded.uri, "spotify:episode:42");
        assert_eq!(loaded.position_ms, 123_000);
        assert_eq!(saved.lock().unwrap().as_ref().unwrap().title, "Выпуск");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn parses_tavily_spotify_urls() {
        let track = item_from_web_url(
            "https://open.spotify.com/track/0dEIca2nhcxDUV8C5QkPYb?si=x",
            "Give Life Back to Music - song and lyrics by Daft Punk",
        )
        .unwrap();
        assert_eq!(track.uri, "spotify:track:0dEIca2nhcxDUV8C5QkPYb");
        assert_eq!(track.name, "Give Life Back to Music");
        assert_eq!(track.kind, "трек");

        let episode = item_from_web_url(
            "https://open.spotify.com/intl-ru/episode/abc123def456",
            "Выпуск 42 | Spotify",
        )
        .unwrap();
        assert_eq!(episode.uri, "spotify:episode:abc123def456");
        assert_eq!(episode.kind, "выпуск");

        assert!(item_from_web_url("https://example.com/track/abc", "x").is_none());
        assert!(item_from_web_url("https://open.spotify.com/artist/abc", "x").is_none());
    }

    #[test]
    fn parses_track_and_show_json() {
        let track = serde_json::json!({
            "uri": "spotify:track:1",
            "name": "Песня",
            "artists": [{"name": "Артист"}],
            "release_date": "2024-01-01"
        });
        let item = parse_item(&track, "трек", "artists", false).unwrap();
        assert_eq!(item.uri, "spotify:track:1");
        assert_eq!(item.subtitle, "Артист");

        let episode = serde_json::json!({
            "uri": "spotify:episode:2",
            "name": "Выпуск",
            "show": {"name": "Подкаст"},
            "release_date": "2026-01-01"
        });
        let item = parse_item(&episode, "выпуск", "show", true).unwrap();
        assert_eq!(item.subtitle, "Подкаст");
        assert_eq!(item.release, "2026-01-01");
    }
}
