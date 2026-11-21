use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::errors::ConfigError;

pub const DEFAULT_CONFIG: &str = "/data/petrovich/config.toml";
pub const DEFAULT_SECRETS: &str = "/data/petrovich/secrets.env";

/// Корневая конфигурация. Неизвестные ключи запрещены: опечатка не должна
/// молча оставлять небезопасное значение по умолчанию.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub wakeword: WakewordConfig,
    #[serde(default)]
    pub commands: CommandsConfig,
    #[serde(default)]
    pub vad: VadConfig,
    #[serde(default)]
    pub dialog: DialogConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub stt: SttConfig,
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub tts: TtsConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub modules: ModulesConfig,
    #[serde(default)]
    pub spotify: SpotifyConfig,
    #[serde(default)]
    pub byedpi: ByeDpiConfig,
    #[serde(default)]
    pub alarms: AlarmConfig,
    #[serde(default)]
    pub indication: IndicationConfig,
    #[serde(default)]
    pub hardware: HardwareConfig,
    #[serde(default)]
    pub privacy: PrivacyConfig,
    #[serde(default)]
    pub network: NetworkConfig,
}

/// Общие сетевые настройки. `proxy` — прокси по умолчанию для всех сетевых
/// компонентов; явный proxy у провайдера или инструмента имеет приоритет.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    #[serde(default)]
    pub proxy: String,
}

impl Config {
    /// Прокси компонента: явный, иначе общий `[network] proxy`.
    pub fn proxy_for<'a>(&'a self, specific: &'a str) -> &'a str {
        if specific.trim().is_empty() {
            self.network.proxy.trim()
        } else {
            specific.trim()
        }
    }
}

/// Необязательные подсистемы. Выключенный модуль не инициализируется и не
/// занимает память: демон не создаёт его клиентов, очередей и фоновых задач.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModulesConfig {
    #[serde(default = "default_true")]
    pub youtube: bool,
    #[serde(default = "default_true")]
    pub web_search: bool,
    #[serde(default = "default_true")]
    pub weather: bool,
    #[serde(default = "default_true")]
    pub nightscout: bool,
    #[serde(default = "default_true")]
    pub display: bool,
    /// Встроенный Spotify Connect (librespot). Без учётных данных модуль
    /// остаётся выключенным, даже если флаг включён.
    #[serde(default = "default_true")]
    pub spotify: bool,
    /// Встроенный обход блокировок YouTube (ByeDPI).
    #[serde(default = "default_true")]
    pub byedpi: bool,
}

impl Default for ModulesConfig {
    fn default() -> Self {
        Self {
            youtube: true,
            web_search: true,
            weather: true,
            nightscout: true,
            display: true,
            spotify: true,
            byedpi: true,
        }
    }
}

/// Встроенный Spotify Connect. Учётные данные — только в secrets.env;
/// `client_id` нужен для поиска через Web API (client credentials flow).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpotifyConfig {
    #[serde(default = "default_spotify_device")]
    pub device_name: String,
    #[serde(default)]
    pub username: String,
    #[serde(default = "default_spotify_password_env")]
    pub password_env: String,
    /// Альтернатива паролю: OAuth access token из secrets.env.
    #[serde(default = "default_spotify_token_env")]
    pub access_token_env: String,
    /// Client ID приложения Spotify для поиска. Пусто — только управление
    /// уже выбранным контентом через Connect.
    #[serde(default)]
    pub client_id: String,
    #[serde(default = "default_spotify_secret_env")]
    pub client_secret_env: String,
    /// Страна для поиска и рекомендаций.
    #[serde(default = "default_spotify_market")]
    pub market: String,
    /// Пусто — `<daemon.state_dir>/spotify`.
    #[serde(default)]
    pub cache_dir: Option<PathBuf>,
    #[serde(default = "default_spotify_bitrate")]
    pub bitrate: u16,
    #[serde(default = "default_spotify_search_limit")]
    pub search_limit: usize,
    /// Прокси librespot и Web API. Поддерживается HTTP(S); пусто — общий
    /// `[network] proxy`, если он HTTP(S).
    #[serde(default)]
    pub proxy: String,
}

impl Default for SpotifyConfig {
    fn default() -> Self {
        Self {
            device_name: default_spotify_device(),
            username: String::new(),
            password_env: default_spotify_password_env(),
            access_token_env: default_spotify_token_env(),
            client_id: String::new(),
            client_secret_env: default_spotify_secret_env(),
            market: default_spotify_market(),
            cache_dir: None,
            bitrate: default_spotify_bitrate(),
            search_limit: default_spotify_search_limit(),
            proxy: String::new(),
        }
    }
}

fn default_spotify_device() -> String {
    "Петрович".into()
}
fn default_spotify_password_env() -> String {
    "PETROVICH_SPOTIFY_PASSWORD".into()
}
fn default_spotify_token_env() -> String {
    "PETROVICH_SPOTIFY_TOKEN".into()
}
fn default_spotify_secret_env() -> String {
    "PETROVICH_SPOTIFY_CLIENT_SECRET".into()
}
fn default_spotify_market() -> String {
    "RU".into()
}
fn default_spotify_bitrate() -> u16 {
    160
}
fn default_spotify_search_limit() -> usize {
    10
}

/// Встроенный обход блокировок YouTube. Прокси слушает только loopback и
/// применяется только к доменам YouTube; системные настройки не меняются.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ByeDpiConfig {
    /// Порт локального SOCKS5; 0 — выбрать свободный.
    #[serde(default)]
    pub listen_port: u16,
    /// Стратегии в порядке предпочтения. Неизвестные имена игнорируются.
    #[serde(default = "default_byedpi_strategies")]
    pub strategies: Vec<String>,
    /// Проверка стратегий не реже этого интервала, секунды.
    #[serde(default = "default_byedpi_refresh_s")]
    pub refresh_s: u64,
    /// Тайм-аут одной проверки, мс.
    #[serde(default = "default_byedpi_probe_ms")]
    pub probe_timeout_ms: u64,
    /// Видео для проверки настоящего медиапотока.
    #[serde(default = "default_byedpi_probe_video")]
    pub probe_video: String,
    /// Пусто — `<daemon.state_dir>/byedpi.json`.
    #[serde(default)]
    pub state_path: Option<PathBuf>,
}

impl Default for ByeDpiConfig {
    fn default() -> Self {
        Self {
            listen_port: 0,
            strategies: default_byedpi_strategies(),
            refresh_s: default_byedpi_refresh_s(),
            probe_timeout_ms: default_byedpi_probe_ms(),
            probe_video: default_byedpi_probe_video(),
            state_path: None,
        }
    }
}

fn default_byedpi_strategies() -> Vec<String> {
    // Пресеты в формате ByeByeDPI: -d split/disorder, -f fake c TTL,
    // -o OOB, -s split. Символические имена тоже принимаются.
    [
        "-d 1 -s 1",
        "-d 2 -s 2",
        "-d 4 -s 4",
        "-d 16 -s 16",
        "-d 1 -o 1",
        "-d 2 -o 2",
        "-d -1 -f -1 -t 1",
        "split_sni",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}
fn default_byedpi_refresh_s() -> u64 {
    300
}
fn default_byedpi_probe_ms() -> u64 {
    4_000
}
fn default_byedpi_probe_video() -> String {
    // Короткий ролик с аудиодорожкой; проверяет и API, и медиасегмент.
    "jNQXAC9IVRw".into()
}

/// Поведение будильников, не зависящее от голосового хода.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AlarmConfig {
    /// Сколько секунд звонить до автоматического отключения, 0 — бесконечно.
    #[serde(default = "default_alarm_ring_s")]
    pub max_ring_s: u64,
    /// Интервал повторного сигнала, секунды.
    #[serde(default = "default_alarm_repeat_s")]
    pub repeat_s: u64,
    /// Отсрочка по умолчанию, минуты.
    #[serde(default = "default_snooze_min")]
    pub snooze_min: u64,
}

impl Default for AlarmConfig {
    fn default() -> Self {
        Self {
            max_ring_s: 300,
            repeat_s: 10,
            snooze_min: 5,
        }
    }
}

fn default_alarm_ring_s() -> u64 {
    300
}
fn default_alarm_repeat_s() -> u64 {
    10
}
fn default_snooze_min() -> u64 {
    5
}

/// Что показывать на четырёхразрядном индикаторе и кольце.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IndicationConfig {
    #[serde(default = "default_true")]
    pub weather: bool,
    #[serde(default = "default_true")]
    pub glucose: bool,
    #[serde(default = "default_true")]
    pub playback: bool,
    #[serde(default = "default_true")]
    pub timer: bool,
    /// Сколько секунд держать погоду/сахар/воспроизведение на экране.
    #[serde(default = "default_indication_hold_s")]
    pub hold_s: u64,
}

impl Default for IndicationConfig {
    fn default() -> Self {
        Self {
            weather: true,
            glucose: true,
            playback: true,
            timer: true,
            hold_s: 6,
        }
    }
}

fn default_indication_hold_s() -> u64 {
    6
}

fn one() -> u32 {
    1
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    #[serde(default = "default_pid_file")]
    pub pid_file: PathBuf,
    #[serde(default = "default_secrets")]
    pub secrets_file: PathBuf,
    /// Дополнительный GID процесса (например, группа прокси 2200).
    #[serde(default)]
    pub run_gid: Option<u32>,
    /// Часовой пояс для часов и ответов, например "Europe/Moscow" или "MSK-3".
    #[serde(default)]
    pub timezone: Option<String>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            log_level: default_log_level(),
            state_dir: default_state_dir(),
            pid_file: default_pid_file(),
            secrets_file: default_secrets(),
            run_gid: None,
            timezone: None,
        }
    }
}

fn default_log_level() -> String {
    "info".into()
}
fn default_state_dir() -> PathBuf {
    PathBuf::from("/data/petrovich/state")
}
fn default_pid_file() -> PathBuf {
    PathBuf::from("/run/petrovichd.pid")
}
fn default_secrets() -> PathBuf {
    PathBuf::from(DEFAULT_SECRETS)
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum VqeMode {
    #[default]
    Auto,
    Off,
    Required,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AudioConfig {
    #[serde(default = "default_pcm")]
    pub capture_pcm: String,
    #[serde(default = "default_pcm")]
    pub playback_pcm: String,
    #[serde(default = "default_capture_rate")]
    pub capture_rate: u32,
    #[serde(default = "default_capture_channels")]
    pub capture_channels: u32,
    #[serde(default = "default_mic_channels")]
    pub mic_channels: Vec<u32>,
    #[serde(default = "default_ref_channels")]
    pub reference_channels: Vec<u32>,
    #[serde(default = "default_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_capture_rate")]
    pub playback_rate: u32,
    #[serde(default = "default_period_ms")]
    pub period_ms: u32,
    #[serde(default)]
    pub vqe: VqeMode,
    #[serde(default = "default_vqe_preset")]
    pub vqe_preset: String,
    #[serde(default = "default_vqe_models")]
    pub vqe_models: PathBuf,
    /// Абсолютный путь к libasound; пусто — обычный поиск загрузчиком.
    #[serde(default)]
    pub alsa_lib: PathBuf,
    /// Дополнительная задержка компенсации тракта динамик→микрофон, мс.
    #[serde(default = "default_aec_delay_ms")]
    pub aec_delay_ms: u32,
    #[serde(default = "default_true")]
    pub aec: bool,
    /// Шумовой затвор в fallback-тракте (VQE делает это сам).
    #[serde(default = "default_true")]
    pub denoise: bool,
    /// Микшировать четыре микрофона в моно до VQE. Для Мини 2 VQE сам
    /// использует все каналы, поэтому по умолчанию false.
    #[serde(default)]
    pub downmix_before_vqe: bool,
    /// Усиление PDM-микрофона карты 0 в процентах; 0 — не менять.
    #[serde(default)]
    pub mic_gain: u8,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            capture_pcm: default_pcm(),
            playback_pcm: default_pcm(),
            capture_rate: default_capture_rate(),
            capture_channels: default_capture_channels(),
            mic_channels: default_mic_channels(),
            reference_channels: default_ref_channels(),
            sample_rate: default_rate(),
            playback_rate: default_capture_rate(),
            period_ms: default_period_ms(),
            vqe: VqeMode::Auto,
            vqe_preset: default_vqe_preset(),
            vqe_models: default_vqe_models(),
            alsa_lib: PathBuf::new(),
            aec_delay_ms: default_aec_delay_ms(),
            aec: true,
            denoise: true,
            downmix_before_vqe: false,
            mic_gain: 0,
        }
    }
}

fn default_pcm() -> String {
    "default".into()
}
fn default_rate() -> u32 {
    16_000
}
fn default_capture_rate() -> u32 {
    48_000
}
fn default_capture_channels() -> u32 {
    6
}
fn default_mic_channels() -> Vec<u32> {
    vec![0, 1, 2, 3]
}
fn default_ref_channels() -> Vec<u32> {
    vec![4, 5]
}
fn default_period_ms() -> u32 {
    20
}
fn default_vqe_preset() -> String {
    "yandexmini_2".into()
}
fn default_vqe_models() -> PathBuf {
    PathBuf::from("/system/vendor/quasar/vqe_models")
}
fn default_aec_delay_ms() -> u32 {
    0
}
pub(crate) fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WakewordConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_wakeword_model")]
    pub model: PathBuf,
    #[serde(default = "default_wakeword_assets")]
    pub assets: PathBuf,
    /// Переопределение порога модели; None — порог из JSON.
    #[serde(default)]
    pub threshold: Option<f32>,
    /// Смещение порога: отрицательное значение делает срабатывание чаще.
    #[serde(default)]
    pub sensitivity: f32,
    #[serde(default = "default_refractory_ms")]
    pub refractory_ms: u64,
    /// Усиление PCM до детектора, как в проверенной конфигурации станции.
    #[serde(default = "default_wake_gain")]
    pub gain: u32,
}

impl Default for WakewordConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model: default_wakeword_model(),
            assets: default_wakeword_assets(),
            threshold: None,
            sensitivity: 0.0,
            refractory_ms: default_refractory_ms(),
            gain: default_wake_gain(),
        }
    }
}

fn default_wakeword_model() -> PathBuf {
    PathBuf::from("/data/petrovich/models/petrovich.tflite")
}
fn default_wakeword_assets() -> PathBuf {
    PathBuf::from("/data/petrovich/assets/oww.bin")
}
fn default_refractory_ms() -> u64 {
    2_000
}
fn default_wake_gain() -> u32 {
    1
}

/// Быстрые команды без слова активации: штатная command-модель прошивки.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_command_model")]
    pub model: PathBuf,
}

impl Default for CommandsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model: default_command_model(),
        }
    }
}

fn default_command_model() -> PathBuf {
    PathBuf::from("/system/vendor/quasar/spotter_models/command")
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VadConfig {
    #[serde(default = "default_min_speech_ms")]
    pub min_speech_ms: u64,
    /// Пауза, после которой заканчивается длинная фраза.
    #[serde(default = "default_silence_ms")]
    pub silence_ms: u64,
    /// Более короткая пауза для короткой команды: «громче», «стоп» и т.п.
    /// не должны ждать полную секунду.
    #[serde(default = "default_short_silence_ms")]
    pub short_silence_ms: u64,
    /// Речь короче этого порога считается короткой командой.
    #[serde(default = "default_short_utterance_ms")]
    pub short_utterance_ms: u64,
    /// Пауза внутри предложения не сбрасывает фразу, но продлевает
    /// ожидание конца. Значение должно быть меньше `silence_ms`.
    #[serde(default = "default_resume_pause_ms")]
    pub resume_pause_ms: u64,
    /// Сколько секунд звука хранить до активации, чтобы поймать
    /// «Петрович, какая погода?» без паузы после имени.
    #[serde(default = "default_activation_pre_roll_ms")]
    pub activation_pre_roll_ms: u64,
    #[serde(default = "default_max_utterance_ms")]
    pub max_utterance_ms: u64,
    #[serde(default = "default_energy_margin_db")]
    pub energy_margin_db: f32,
    #[serde(default = "default_pre_roll_ms")]
    pub pre_roll_ms: u64,
    /// Абсолютный потолок: более громкий звук считается речью даже в шуме.
    #[serde(default = "default_absolute_db")]
    pub absolute_db: f32,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            min_speech_ms: default_min_speech_ms(),
            silence_ms: default_silence_ms(),
            short_silence_ms: default_short_silence_ms(),
            short_utterance_ms: default_short_utterance_ms(),
            resume_pause_ms: default_resume_pause_ms(),
            activation_pre_roll_ms: default_activation_pre_roll_ms(),
            max_utterance_ms: default_max_utterance_ms(),
            energy_margin_db: default_energy_margin_db(),
            pre_roll_ms: default_pre_roll_ms(),
            absolute_db: default_absolute_db(),
        }
    }
}

fn default_min_speech_ms() -> u64 {
    300
}
fn default_silence_ms() -> u64 {
    // Длинная фраза: даём договорить после паузы на размышление.
    2_200
}
fn default_short_silence_ms() -> u64 {
    // Даже короткая команда не обрывается на паузе внутри предложения.
    1_400
}
fn default_short_utterance_ms() -> u64 {
    4_000
}
fn default_resume_pause_ms() -> u64 {
    1_100
}
fn default_activation_pre_roll_ms() -> u64 {
    2_500
}
fn default_max_utterance_ms() -> u64 {
    45_000
}
fn default_energy_margin_db() -> f32 {
    9.0
}
fn default_pre_roll_ms() -> u64 {
    300
}
fn default_absolute_db() -> f32 {
    // Нижний предел порога: тихая речь не должна теряться в «абсолютной»
    // отсечке. Шум оценивается отдельно и поднимает порог при необходимости.
    -55.0
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DialogConfig {
    #[serde(default = "default_follow_up_ms")]
    pub follow_up_ms: u64,
    /// Сколько ждать начала речи после активации.
    #[serde(default = "default_listen_timeout_ms")]
    pub listen_timeout_ms: u64,
    #[serde(default = "default_history_turns")]
    pub history_turns: usize,
    #[serde(default = "default_history_chars")]
    pub history_chars: usize,
    #[serde(default = "default_history_ttl")]
    pub history_ttl_s: u64,
    #[serde(default = "default_true")]
    pub barge_in: bool,
    /// Перебивание голосом поверх ответа. По умолчанию выключено: без
    /// надёжного эхоподавления TTS принимается за речь.
    #[serde(default)]
    pub barge_in_voice: bool,
    #[serde(default = "default_true")]
    pub wake_chime: bool,
    /// Звуковые подтверждения «услышал», «готово», «остановлено»,
    /// «диалог очищен».
    #[serde(default = "default_true")]
    pub state_chimes: bool,
    #[serde(default = "default_response_timeout_ms")]
    pub response_timeout_ms: u64,
    #[serde(default = "default_max_tool_rounds")]
    pub max_tool_rounds: u8,
}

impl Default for DialogConfig {
    fn default() -> Self {
        Self {
            follow_up_ms: default_follow_up_ms(),
            listen_timeout_ms: default_listen_timeout_ms(),
            history_turns: default_history_turns(),
            history_chars: default_history_chars(),
            history_ttl_s: default_history_ttl(),
            barge_in: true,
            barge_in_voice: false,
            wake_chime: true,
            state_chimes: true,
            response_timeout_ms: default_response_timeout_ms(),
            max_tool_rounds: default_max_tool_rounds(),
        }
    }
}

fn default_follow_up_ms() -> u64 {
    5_000
}
fn default_listen_timeout_ms() -> u64 {
    10_000
}
fn default_history_turns() -> usize {
    8
}
fn default_history_chars() -> usize {
    12_000
}
fn default_history_ttl() -> u64 {
    900
}
fn default_response_timeout_ms() -> u64 {
    // Ход включает не только LLM, но и синтез с воспроизведением всех фраз.
    // Минуты хватает только на короткий ответ; подробный ответ обрывался.
    180_000
}
fn default_max_tool_rounds() -> u8 {
    20
}

/// Долговременная память: короткие факты о пользователе, переживающие
/// сброс диалога и перезапуск.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Пусто — `<daemon.state_dir>/memory.json`.
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default = "default_memory_entries")]
    pub max_entries: usize,
    #[serde(default = "default_memory_chars")]
    pub max_chars: usize,
    /// Сколько символов памяти добавлять в системный промпт.
    #[serde(default = "default_memory_prompt_chars")]
    pub prompt_chars: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: None,
            max_entries: default_memory_entries(),
            max_chars: default_memory_chars(),
            prompt_chars: default_memory_prompt_chars(),
        }
    }
}

fn default_memory_entries() -> usize {
    200
}
fn default_memory_chars() -> usize {
    8_000
}
fn default_memory_prompt_chars() -> usize {
    2_000
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SttProvider {
    Openai,
    Fish,
    /// Распознавание через мультимодальную chat-модель (аудио во входе).
    #[serde(rename = "chat_audio")]
    ChatAudio,
    Command,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SttConfig {
    #[serde(default = "default_stt_provider")]
    pub provider: SttProvider,
    #[serde(default = "default_stt_url")]
    pub base_url: String,
    #[serde(default = "default_stt_model")]
    pub model: String,
    #[serde(default = "default_stt_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default = "default_http_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_retries")]
    pub retries: u8,
    /// Для локального STT: argv команды; WAV подаётся на stdin.
    #[serde(default)]
    pub command: Vec<String>,
    /// Дополнительный корневой сертификат для self-hosted HTTPS.
    #[serde(default)]
    pub tls_ca_file: Option<PathBuf>,
    /// Прокси только для STT, например socks5h://127.0.0.1:1080.
    #[serde(default)]
    pub proxy: String,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            provider: default_stt_provider(),
            base_url: default_stt_url(),
            model: default_stt_model(),
            api_key_env: default_stt_key_env(),
            language: default_language(),
            timeout_ms: default_http_timeout_ms(),
            retries: default_retries(),
            command: Vec::new(),
            tls_ca_file: None,
            proxy: String::new(),
        }
    }
}

fn default_stt_provider() -> SttProvider {
    SttProvider::Openai
}
fn default_stt_url() -> String {
    "https://api.openai.com/v1".into()
}
fn default_stt_model() -> String {
    "whisper-1".into()
}
fn default_stt_key_env() -> String {
    "PETROVICH_STT_API_KEY".into()
}
fn default_language() -> String {
    "ru".into()
}
fn default_http_timeout_ms() -> u64 {
    30_000
}
fn default_retries() -> u8 {
    2
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LlmConfig {
    #[serde(default = "default_llm_url")]
    pub base_url: String,
    #[serde(default = "default_llm_model")]
    pub model: String,
    /// Модели, которые пробуются по порядку, если основная вернула 429,
    /// 404 или отказ разбора вызова инструментов.
    #[serde(default)]
    pub fallback_models: Vec<String>,
    #[serde(default = "default_llm_key_env")]
    pub api_key_env: String,
    /// Дополнительные переменные с ключами того же провайдера. При 429/401
    /// ключи перебираются до смены модели.
    #[serde(default)]
    pub api_key_envs: Vec<String>,
    #[serde(default = "default_system_prompt")]
    pub system_prompt: String,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_llm_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_retries")]
    pub retries: u8,
    #[serde(default = "default_true")]
    pub stream: bool,
    #[serde(default = "default_true")]
    pub tools: bool,
    /// Прокси только для LLM, например socks5h://127.0.0.1:1080.
    #[serde(default)]
    pub proxy: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: default_llm_url(),
            model: default_llm_model(),
            fallback_models: Vec::new(),
            api_key_env: default_llm_key_env(),
            api_key_envs: Vec::new(),
            system_prompt: default_system_prompt(),
            temperature: default_temperature(),
            max_tokens: default_max_tokens(),
            timeout_ms: default_llm_timeout_ms(),
            retries: default_retries(),
            stream: true,
            tools: true,
            proxy: String::new(),
        }
    }
}

fn default_llm_url() -> String {
    "https://api.openai.com/v1".into()
}
fn default_llm_model() -> String {
    "gpt-4o-mini".into()
}
fn default_llm_key_env() -> String {
    "PETROVICH_LLM_API_KEY".into()
}
fn default_temperature() -> f32 {
    0.4
}
fn default_max_tokens() -> u32 {
    800
}
fn default_llm_timeout_ms() -> u64 {
    60_000
}
pub fn default_system_prompt() -> String {
    r#"Ты — Петрович, голос в колонке. Для собеседника ты не сервис и не «ассистент», а дерзкий
друг: прямой, ироничный, с ленцой, но надёжный. Подкалывай по-доброму, спорь, если человек несёт
чушь, но дело делай. Говори по-русски.

Как говорить.
Отвечай голосом: одна-две короткие фразы, до 25 слов. Без списков, разметки, эмодзи, сносок
и скобок. Никогда не заканчивай ответ служебными фразами: «если что-то ещё нужно — обращайтесь»,
«чем ещё могу помочь», «всегда рад», «с удовольствием», «к сожалению», «извините за неудобства».
Не отчитывайся как робот: вместо «задача успешно выполнена» — «Сделал», «Уже играет», «Поставил».
Не переспрашивай без нужды: выбери разумный вариант и сделай. Уточняй, только если вариантов
правда много и выбор важен. Не извиняйся трижды, не благодари за вопрос, не говори «отличный
вопрос». Не называй себя ассистентом, помощником или ИИ — ты Петрович. Дерзость в тоне, не в мате.

Честность.
Не говори «включаю», «готово», «поставил», пока инструмент не вернул успех. Сказал — значит
сделал. Инструмент упал — коротко скажи, что именно не вышло, без драмы и без выдуманных причин.

Дела.
Сложное поручение разбивай на шаги: вызови инструменты, изучи результаты, уточни запрос, сравни
варианты и только потом действуй. Независимые действия (найти выпуск, поставить таймер или
будильник) выполняй в одном раунде. Если один инструмент упал, выполняй остальные и честно скажи,
что не вышло. Для YouTube используй поиск с метаданными и страницами, для длинных записей —
субтитры и комментарии; не бери первый результат, если он не совпадает с просьбой.
Для Spotify используй инструмент music: play ищет трек, альбом, плейлист, подкаст или выпуск;
доступны pause, resume, next, previous, status, seek, queue, sleep, save, continue. Если просили
Spotify, не подменяй его YouTube: при ошибке скажи прямо и не утверждай, что музыка играет.
«Мои любимые треки», «мне нравится», «залайканные» — это music liked, коллекция без названия:
вызывай сразу и не спрашивай, как называется плейлист. «Продолжай музыку» — music resume
или continue. «Замолчи», «стоп», «хватит», «выключайся» —
вызови stop; музыку при этом не останавливай, если не просили.
Пришла бессмысленная короткая фраза или шум — ничего не выключай, не переключай и не ставь
на паузу; переспроси одной фразой: «Чего?» или «Не разобрал».

Память.
Важный факт о собеседнике (имя, город, привычки, предпочтения) или просьбу запомнить сохраняй
инструментом remember; просьбу забыть — инструментом forget. Пароли, ключи и другие секреты
не сохраняй. Город для погоды храни как «Город: X»: если не знаешь — спроси один раз и запомни,
при переезде замени старый.

Код, команды оболочки, SQL и конфигурацию не пишешь. Просят — ответь ровно: «Код не пишу.»"#
        .into()
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TtsProvider {
    Openai,
    Fish,
    Command,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TtsFormat {
    Pcm,
    Wav,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TtsConfig {
    #[serde(default = "default_tts_provider")]
    pub provider: TtsProvider,
    #[serde(default = "default_tts_url")]
    pub base_url: String,
    #[serde(default = "default_tts_model")]
    pub model: String,
    #[serde(default = "default_tts_voice")]
    pub voice: String,
    #[serde(default = "default_tts_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_tts_format")]
    pub format: TtsFormat,
    #[serde(default = "default_tts_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_http_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_retries")]
    pub retries: u8,
    /// Максимальный размер одного ответа TTS, байт.
    #[serde(default = "default_tts_limit")]
    pub max_bytes: usize,
    /// Для локального TTS: argv команды; текст подаётся на stdin, PCM/WAV — на stdout.
    #[serde(default)]
    pub command: Vec<String>,
    /// Прокси TTS; пусто — общий `[network] proxy`.
    #[serde(default)]
    pub proxy: String,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            provider: default_tts_provider(),
            base_url: default_tts_url(),
            model: default_tts_model(),
            voice: default_tts_voice(),
            api_key_env: default_tts_key_env(),
            format: TtsFormat::Pcm,
            sample_rate: default_tts_rate(),
            timeout_ms: default_http_timeout_ms(),
            retries: default_retries(),
            max_bytes: default_tts_limit(),
            command: Vec::new(),
            proxy: String::new(),
        }
    }
}

fn default_tts_provider() -> TtsProvider {
    TtsProvider::Openai
}
fn default_tts_url() -> String {
    "https://api.openai.com/v1".into()
}
fn default_tts_model() -> String {
    "gpt-4o-mini-tts".into()
}
fn default_tts_voice() -> String {
    "alloy".into()
}
fn default_tts_key_env() -> String {
    "PETROVICH_TTS_API_KEY".into()
}
fn default_tts_format() -> TtsFormat {
    TtsFormat::Pcm
}
fn default_tts_rate() -> u32 {
    24_000
}
fn default_tts_limit() -> usize {
    16 * 1024 * 1024
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolsConfig {
    #[serde(default = "default_tools_enabled")]
    pub enabled: Vec<String>,
    #[serde(default = "default_tool_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_web_timeout_ms")]
    pub web_timeout_ms: u64,
    #[serde(default = "default_web_max_bytes")]
    pub web_max_bytes: usize,
    /// Прокси только для поиска, например socks5h://127.0.0.1:1080.
    /// Нужен, если основной трафик идёт через общий прокси.
    #[serde(default)]
    pub search_proxy: String,
    /// Переменная secrets.env с ключом Tavily; непусто — поиск через Tavily.
    #[serde(default)]
    pub search_api_key_env: String,
    /// Разрешить web_fetch к частным адресам и HTTP. Только для отладки.
    #[serde(default)]
    pub web_allow_private: bool,
    /// Open-Meteo или совместимый endpoint. Пусто — инструмент погоды выключен.
    #[serde(default = "default_weather_url")]
    pub weather_url: String,
    /// Геокодинг для погоды в произвольном месте; пусто — только координаты.
    #[serde(default = "default_weather_geocoding_url")]
    pub weather_geocoding_url: String,
    #[serde(default)]
    pub weather_latitude: Option<f64>,
    #[serde(default)]
    pub weather_longitude: Option<f64>,
    /// Название места для ответа, например «Москва».
    #[serde(default)]
    pub weather_place: String,
    /// Базовый URL Nightscout. Пусто — инструмент sugar выключен.
    #[serde(default)]
    pub nightscout_url: String,
    /// Переменная secrets.env с access token Nightscout; пусто — без токена.
    #[serde(default)]
    pub nightscout_token_env: String,
    /// Песочница для run_code; пусто — инструмент отключён независимо от enabled.
    #[serde(default)]
    pub sandbox_command: Vec<String>,
    #[serde(default = "default_sandbox_timeout_ms")]
    pub sandbox_timeout_ms: u64,
    /// Команда управления музыкой, например ["spotiya"].
    #[serde(default)]
    pub music_command: Vec<String>,
    /// Прокси для YouTube, обычно byedpi: socks5h://127.0.0.1:1080.
    /// Пусто — прямое подключение. Поддерживаются socks5(h) и http(s).
    #[serde(default)]
    pub youtube_proxy: String,
    /// Управление домашним ПК; отсутствие секции выключает инструмент.
    #[serde(default)]
    pub computer: Option<ComputerConfig>,
}

/// Домашний ПК, которым управляет колонка. Включение и пробуждение — это
/// magic packet (WOL); остальные действия идут одной SSH-командой на
/// ограниченный endpoint ПК. Свободных команд и оболочки нет.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ComputerConfig {
    /// Адрес ПК; закрепляется в роутере.
    pub host: String,
    /// MAC для magic packet, например "fc:34:97:17:d1:00".
    pub mac: String,
    /// Broadcast-адрес локальной сети, например "192.168.3.255".
    pub broadcast: String,
    #[serde(default = "default_wake_port")]
    pub wake_port: u16,
    #[serde(default = "default_ssh_port")]
    pub ssh_port: u16,
    /// Клиент SSH (dropbear dbclient): на прошивке нет openssh-client.
    pub ssh_bin: String,
    /// Приватный ключ для ограниченного endpoint ПК.
    pub key: String,
    /// HOME для dbclient: в `<home>/.ssh/known_hosts` лежит ключ хоста ПК.
    #[serde(default = "default_computer_home")]
    pub home: String,
    #[serde(default = "default_computer_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_wake_port() -> u16 {
    9
}
fn default_ssh_port() -> u16 {
    2222
}
fn default_computer_home() -> String {
    "/data/petrovich/pc".into()
}
fn default_computer_timeout_ms() -> u64 {
    12_000
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            enabled: default_tools_enabled(),
            timeout_ms: default_tool_timeout_ms(),
            web_timeout_ms: default_web_timeout_ms(),
            web_max_bytes: default_web_max_bytes(),
            search_proxy: String::new(),
            search_api_key_env: String::new(),
            web_allow_private: false,
            weather_url: default_weather_url(),
            weather_geocoding_url: default_weather_geocoding_url(),
            weather_latitude: None,
            weather_longitude: None,
            weather_place: String::new(),
            nightscout_url: String::new(),
            nightscout_token_env: String::new(),
            sandbox_command: Vec::new(),
            sandbox_timeout_ms: default_sandbox_timeout_ms(),
            music_command: Vec::new(),
            youtube_proxy: String::new(),
            computer: None,
        }
    }
}

fn default_tools_enabled() -> Vec<String> {
    [
        "time",
        "timer",
        "alarm",
        "volume",
        "music",
        "youtube",
        "web_search",
        "weather",
        "sugar",
        "emotion",
        "web_fetch",
        "remember",
        "forget",
        "reset_dialog",
        "stop",
        "computer",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}
fn default_tool_timeout_ms() -> u64 {
    10_000
}
fn default_web_timeout_ms() -> u64 {
    15_000
}
fn default_weather_url() -> String {
    "https://api.open-meteo.com/v1/forecast".into()
}
fn default_weather_geocoding_url() -> String {
    "https://geocoding-api.open-meteo.com/v1/search".into()
}
fn default_web_max_bytes() -> usize {
    1024 * 1024
}
fn default_sandbox_timeout_ms() -> u64 {
    5_000
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HardwareConfig {
    /// "auto" или кодовое имя: station-mini2, ...
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_true")]
    pub leds: bool,
    #[serde(default = "default_true")]
    pub clock: bool,
    #[serde(default = "default_true")]
    pub buttons: bool,
    /// Переопределение GPIO аппаратного мута; None — значение профиля.
    #[serde(default)]
    pub mic_mute_gpio: Option<u32>,
    #[serde(default)]
    pub input_device: Option<String>,
    #[serde(default = "default_action_key")]
    pub action_key: u16,
    #[serde(default = "default_volume_step")]
    pub volume_step: i32,
    #[serde(default = "default_led_brightness")]
    pub led_brightness: f32,
    #[serde(default = "default_clock_brightness")]
    pub clock_brightness: f32,
    /// Подстраивать яркость кольца и часов по датчику освещённости.
    #[serde(default = "default_true")]
    pub auto_brightness: bool,
    /// Каталог IIO-устройства или файл значения; None — автопоиск ltr308.
    #[serde(default)]
    pub light_sensor: Option<PathBuf>,
    /// Минимальная доля яркости ночью (0..1).
    #[serde(default = "default_night_brightness")]
    pub night_brightness: f32,
}

impl Default for HardwareConfig {
    fn default() -> Self {
        Self {
            model: default_model(),
            leds: true,
            clock: true,
            buttons: true,
            mic_mute_gpio: None,
            input_device: None,
            action_key: default_action_key(),
            volume_step: default_volume_step(),
            led_brightness: default_led_brightness(),
            clock_brightness: default_clock_brightness(),
            auto_brightness: true,
            light_sensor: None,
            night_brightness: default_night_brightness(),
        }
    }
}

fn default_model() -> String {
    "auto".into()
}
fn default_action_key() -> u16 {
    582
}
fn default_volume_step() -> i32 {
    5
}
fn default_led_brightness() -> f32 {
    0.8
}
fn default_clock_brightness() -> f32 {
    0.35
}
fn default_night_brightness() -> f32 {
    0.15
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrivacyConfig {
    /// Диагностика проверяет, что после старта нет соединений с Яндексом.
    #[serde(default = "default_true")]
    pub check_yandex_connections: bool,
    /// Известные домены Яндекса для проверки DNS/соединений.
    #[serde(default = "default_yandex_hosts")]
    pub yandex_hosts: Vec<String>,
}

impl Default for PrivacyConfig {
    fn default() -> Self {
        Self {
            check_yandex_connections: true,
            yandex_hosts: default_yandex_hosts(),
        }
    }
}

fn default_yandex_hosts() -> Vec<String> {
    [
        "yandex.net",
        "yandex.ru",
        "yandex.com",
        "alice.yandex.net",
        "uniproxy.alice.yandex.net",
        "clck.yandex.net",
        "quasar.yandex.net",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

/// Секреты читаются из отдельного файла и не попадают в `Debug`/логи.
#[derive(Clone, Default)]
pub struct Secrets(BTreeMap<String, String>);

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secrets")
            .field("names", &self.0.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl Secrets {
    pub fn load(path: &Path) -> std::result::Result<Self, ConfigError> {
        let meta = fs::metadata(path).map_err(|source| ConfigError::SecretRead {
            path: path.to_path_buf(),
            source,
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(ConfigError::SecretPermissions {
                    path: path.to_path_buf(),
                    mode,
                });
            }
        }
        let text = fs::read_to_string(path).map_err(|source| ConfigError::SecretRead {
            path: path.to_path_buf(),
            source,
        })?;
        let mut out = BTreeMap::new();
        for (lineno, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line.split_once('=').ok_or_else(|| {
                ConfigError::Invalid(format!(
                    "{}:{}: ожидается KEY=VALUE",
                    path.display(),
                    lineno + 1
                ))
            })?;
            let key = key.trim();
            if key.is_empty()
                || !key
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            {
                return Err(ConfigError::Invalid(format!(
                    "{}:{}: недопустимое имя переменной",
                    path.display(),
                    lineno + 1
                )));
            }
            out.insert(key.to_owned(), value.trim().to_owned());
        }
        Ok(Self(out))
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    pub fn require(&self, name: &str, path: &Path) -> std::result::Result<&str, ConfigError> {
        self.get(name)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| ConfigError::MissingSecret {
                env: name.to_owned(),
                path: path.to_path_buf(),
            })
    }

    /// Только имена: значения не должны попасть в журнал.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }
}

#[derive(Clone)]
pub struct LoadedConfig {
    pub config: Config,
    pub secrets: Secrets,
    pub path: PathBuf,
    pub secrets_path: PathBuf,
}

impl Config {
    pub fn load(path: &Path) -> std::result::Result<LoadedConfig, ConfigError> {
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let config: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        config.validate()?;
        let secrets_path = config.daemon.secrets_file.clone();
        let secrets = if secrets_path.exists() {
            Secrets::load(&secrets_path)?
        } else {
            Secrets::default()
        };
        Ok(LoadedConfig {
            config,
            secrets,
            path: path.to_path_buf(),
            secrets_path,
        })
    }

    /// Файл долговременной памяти: явный путь или `state_dir/memory.json`.
    pub fn memory_path(&self) -> PathBuf {
        self.memory
            .path
            .clone()
            .unwrap_or_else(|| self.daemon.state_dir.join("memory.json"))
    }

    /// Файл сохранённых таймеров.
    pub fn timer_state_path(&self) -> PathBuf {
        self.daemon.state_dir.join("timers.json")
    }

    /// Файл сохранённых будильников.
    pub fn alarm_state_path(&self) -> PathBuf {
        self.daemon.state_dir.join("alarms.json")
    }

    /// Позиция последнего видео YouTube.
    pub fn youtube_state_path(&self) -> PathBuf {
        self.daemon.state_dir.join("youtube.json")
    }

    /// Выбранная стратегия ByeDPI.
    pub fn byedpi_state_path(&self) -> PathBuf {
        self.byedpi
            .state_path
            .clone()
            .unwrap_or_else(|| self.daemon.state_dir.join("byedpi.json"))
    }

    /// Кэш librespot: учётные данные, метаданные, аудио.
    pub fn spotify_cache_dir(&self) -> PathBuf {
        self.spotify
            .cache_dir
            .clone()
            .unwrap_or_else(|| self.daemon.state_dir.join("spotify"))
    }

    pub fn validate(&self) -> std::result::Result<(), ConfigError> {
        let invalid = |m: String| Err(ConfigError::Invalid(m));
        if self.version != 1 {
            return invalid(format!(
                "версия конфигурации {} не поддерживается (ожидается 1)",
                self.version
            ));
        }
        if self.audio.capture_channels == 0 {
            return invalid("audio.capture_channels не может быть 0".into());
        }
        if self.audio.sample_rate < 8_000 || self.audio.sample_rate > 48_000 {
            return invalid("audio.sample_rate должен быть 8000..48000".into());
        }
        if self.audio.capture_rate == 0 || self.audio.playback_rate == 0 {
            return invalid("частоты audio не могут быть 0".into());
        }
        if self.audio.capture_rate % self.audio.sample_rate != 0 {
            return invalid(format!(
                "audio.capture_rate ({}) должен делиться на sample_rate ({})",
                self.audio.capture_rate, self.audio.sample_rate
            ));
        }
        if self.audio.mic_channels.is_empty() {
            return invalid("audio.mic_channels пуст".into());
        }
        for ch in self
            .audio
            .mic_channels
            .iter()
            .chain(&self.audio.reference_channels)
        {
            if *ch >= self.audio.capture_channels {
                return invalid(format!(
                    "канал {ch} вне диапазона capture_channels={}",
                    self.audio.capture_channels
                ));
            }
        }
        if self.audio.period_ms == 0 || self.audio.period_ms > 100 {
            return invalid("audio.period_ms должен быть 1..100".into());
        }
        if self.wakeword.enabled && self.wakeword.gain > 64 {
            return invalid("wakeword.gain не должен превышать 64 (клиппинг)".into());
        }
        if self.commands.enabled {
            if self.commands.model.as_os_str().is_empty() {
                return invalid("commands.model пуст".into());
            }
            if !self.commands.model.is_absolute() {
                return invalid("commands.model должен быть абсолютным путём".into());
            }
        }
        if self.vad.min_speech_ms == 0 || self.vad.silence_ms == 0 {
            return invalid("vad.min_speech_ms и vad.silence_ms должны быть > 0".into());
        }
        if self.vad.max_utterance_ms < self.vad.min_speech_ms {
            return invalid("vad.max_utterance_ms меньше min_speech_ms".into());
        }
        if self.vad.short_silence_ms == 0
            || self.vad.short_silence_ms > self.vad.silence_ms
            || self.vad.short_utterance_ms == 0
            || self.vad.resume_pause_ms >= self.vad.silence_ms
            || self.vad.activation_pre_roll_ms > 10_000
        {
            return invalid(
                "vad: short_silence_ms должен быть 1..silence_ms, short_utterance_ms > 0,                  resume_pause_ms < silence_ms, activation_pre_roll_ms <= 10000"
                    .into(),
            );
        }
        if self.dialog.listen_timeout_ms < 1_000 {
            return invalid("dialog.listen_timeout_ms должен быть не меньше 1000".into());
        }
        if self.dialog.history_turns == 0 {
            return invalid("dialog.history_turns должен быть > 0".into());
        }
        if self.dialog.max_tool_rounds > 20 {
            return invalid("dialog.max_tool_rounds не должен превышать 20".into());
        }
        if self.modules.spotify {
            if self.spotify.device_name.trim().is_empty() {
                return invalid("spotify.device_name пуст".into());
            }
            if !matches!(self.spotify.bitrate, 96 | 160 | 320) {
                return invalid("spotify.bitrate должен быть 96, 160 или 320".into());
            }
            if self.spotify.search_limit == 0 || self.spotify.search_limit > 50 {
                return invalid("spotify.search_limit должен быть 1..50".into());
            }
            if !self.spotify.client_id.is_empty()
                && self.spotify.client_secret_env.trim().is_empty()
            {
                return invalid(
                    "spotify.client_id задан, но spotify.client_secret_env пуст".into(),
                );
            }
        }
        if self.modules.byedpi && self.byedpi.strategies.is_empty() {
            return invalid("byedpi.strategies пуст: нечего проверять".into());
        }
        if self.modules.byedpi && self.byedpi.probe_video.trim().is_empty() {
            return invalid("byedpi.probe_video пуст".into());
        }
        if self.byedpi.refresh_s < 30 {
            return invalid("byedpi.refresh_s должен быть не меньше 30".into());
        }
        if self.alarms.repeat_s == 0 || self.alarms.snooze_min == 0 {
            return invalid("alarms.repeat_s и alarms.snooze_min должны быть > 0".into());
        }
        if self.indication.hold_s == 0 || self.indication.hold_s > 60 {
            return invalid("indication.hold_s должен быть 1..60".into());
        }
        if self.memory.max_entries == 0 || self.memory.max_entries > 1_000 {
            return invalid("memory.max_entries должен быть 1..1000".into());
        }
        if self.memory.max_chars < 256 {
            return invalid("memory.max_chars должен быть не меньше 256".into());
        }
        if self.memory.prompt_chars < 100 || self.memory.prompt_chars > self.memory.max_chars {
            return invalid("memory.prompt_chars должен быть 100..max_chars".into());
        }
        if self.tools.enabled.iter().any(|t| t.trim().is_empty()) {
            return invalid("tools.enabled содержит пустое имя".into());
        }
        if !self.tools.youtube_proxy.trim().is_empty() {
            let p = self.tools.youtube_proxy.trim();
            if !(p.starts_with("socks5h://")
                || p.starts_with("socks5://")
                || p.starts_with("http://")
                || p.starts_with("https://"))
            {
                return invalid(
                    "tools.youtube_proxy должен начинаться с socks5h://, socks5://, http:// или https://"
                        .into(),
                );
            }
        }
        if let Some(pc) = &self.tools.computer {
            if pc.host.trim().is_empty() {
                return invalid("tools.computer.host пуст".into());
            }
            if parse_mac(&pc.mac).is_none() {
                return invalid("tools.computer.mac должен быть MAC из шести байт".into());
            }
            if pc.broadcast.parse::<std::net::Ipv4Addr>().is_err() {
                return invalid("tools.computer.broadcast должен быть IPv4-адресом".into());
            }
            if pc.wake_port == 0 || pc.ssh_port == 0 {
                return invalid("tools.computer.wake_port/ssh_port не могут быть 0".into());
            }
            for (field, value) in [
                ("ssh_bin", &pc.ssh_bin),
                ("key", &pc.key),
                ("home", &pc.home),
            ] {
                if !Path::new(value).is_absolute() {
                    return invalid(format!(
                        "tools.computer.{field} должен быть абсолютным путём"
                    ));
                }
            }
            if !(1_000..=60_000).contains(&pc.timeout_ms) {
                return invalid("tools.computer.timeout_ms должен быть 1000..60000".into());
            }
        }
        if !(0.0..=1.0).contains(&self.hardware.led_brightness) {
            return invalid("hardware.led_brightness должен быть 0..1".into());
        }
        if !(0.0..=1.0).contains(&self.hardware.clock_brightness) {
            return invalid("hardware.clock_brightness должен быть 0..1".into());
        }
        if !(0.0..=1.0).contains(&self.hardware.night_brightness) {
            return invalid("hardware.night_brightness должен быть 0..1".into());
        }
        if !(1..=50).contains(&self.hardware.volume_step) {
            return invalid("hardware.volume_step должен быть 1..50".into());
        }
        match self.stt.provider {
            SttProvider::Openai | SttProvider::Fish | SttProvider::ChatAudio => {
                validate_http_url("stt.base_url", &self.stt.base_url)?;
                if self.stt.model.is_empty() {
                    return invalid("stt.model пуст".into());
                }
                if let Some(ca) = &self.stt.tls_ca_file {
                    if !ca.is_file() {
                        return invalid(format!("stt.tls_ca_file не найден: {}", ca.display()));
                    }
                }
            }
            SttProvider::Command => {
                if self.stt.command.is_empty() {
                    return invalid("stt.provider=command, но stt.command пуст".into());
                }
            }
        }
        for (name, proxy) in [
            ("network.proxy", &self.network.proxy),
            ("stt.proxy", &self.stt.proxy),
            ("llm.proxy", &self.llm.proxy),
            ("tts.proxy", &self.tts.proxy),
            ("spotify.proxy", &self.spotify.proxy),
        ] {
            let p = proxy.trim();
            if !p.is_empty()
                && !(p.starts_with("socks5h://")
                    || p.starts_with("socks5://")
                    || p.starts_with("http://")
                    || p.starts_with("https://"))
            {
                return invalid(format!(
                    "{name} должен начинаться с socks5h://, socks5://, http:// или https://"
                ));
            }
        }
        validate_http_url("llm.base_url", &self.llm.base_url)?;
        if self.llm.model.is_empty() {
            return invalid("llm.model пуст".into());
        }
        if self.llm.temperature < 0.0 || self.llm.temperature > 2.0 {
            return invalid("llm.temperature должен быть 0..2".into());
        }
        if self
            .llm
            .api_key_envs
            .iter()
            .any(|env| env.trim().is_empty())
        {
            return invalid("llm.api_key_envs содержит пустое имя".into());
        }
        match self.tts.provider {
            TtsProvider::Openai | TtsProvider::Fish => {
                validate_http_url("tts.base_url", &self.tts.base_url)?;
                if self.tts.model.is_empty() || self.tts.voice.is_empty() {
                    return invalid("tts.model/voice пусты".into());
                }
            }
            TtsProvider::Command => {
                if self.tts.command.is_empty() {
                    return invalid("tts.provider=command, но tts.command пуст".into());
                }
            }
        }
        if !self.tools.search_proxy.trim().is_empty() {
            let p = self.tools.search_proxy.trim();
            if !(p.starts_with("socks5h://")
                || p.starts_with("socks5://")
                || p.starts_with("http://")
                || p.starts_with("https://"))
            {
                return invalid(
                    "tools.search_proxy должен начинаться с socks5h://, socks5://, http:// или https://"
                        .into(),
                );
            }
        }
        if !self.tools.nightscout_url.is_empty() {
            validate_http_url("tools.nightscout_url", &self.tools.nightscout_url)?;
        }
        if !self.tools.weather_url.is_empty() {
            validate_http_url("tools.weather_url", &self.tools.weather_url)?;
            match (self.tools.weather_latitude, self.tools.weather_longitude) {
                (Some(lat), Some(lon)) if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) => {}
                (None, None) => {}
                _ => {
                    return invalid(
                        "tools.weather_latitude/longitude должны быть заданы вместе и в допустимых пределах"
                            .into(),
                    )
                }
            }
        }
        Ok(())
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }
}

/// Разбирает MAC из `aa:bb:cc:dd:ee:ff`; разделители `:` и `-` допустимы.
pub fn parse_mac(value: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = value.split([':', '-']).collect();
    if parts.len() != 6 {
        return None;
    }
    let mut out = [0u8; 6];
    for (slot, part) in out.iter_mut().zip(parts) {
        if part.len() != 2 {
            return None;
        }
        *slot = u8::from_str_radix(part, 16).ok()?;
    }
    Some(out)
}

fn validate_http_url(field: &str, value: &str) -> std::result::Result<(), ConfigError> {
    let url = Url::parse(value)
        .map_err(|e| ConfigError::Invalid(format!("{field}: неверный URL: {e}")))?;
    let loopback = url.host_str().is_some_and(|h| {
        h == "localhost" || h == "127.0.0.1" || h == "::1" || h.ends_with(".localhost")
    });
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        "http" => {
            return Err(ConfigError::Invalid(format!(
                "{field}: открытый HTTP запрещён, нужен HTTPS (или loopback для локального сервиса)"
            )))
        }
        other => {
            return Err(ConfigError::Invalid(format!(
                "{field}: схема {other} не поддерживается"
            )))
        }
    }
    if url.host_str().is_none() {
        return Err(ConfigError::Invalid(format!("{field}: нет хоста")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> Config {
        toml::from_str(include_str!("../../../config/config.example.toml")).expect("example")
    }

    #[test]
    fn example_config_is_valid_and_roundtrips() {
        let cfg = example();
        cfg.validate().expect("valid");
        let text = cfg.to_toml();
        let again: Config = toml::from_str(&text).expect("roundtrip");
        assert_eq!(again.audio.sample_rate, cfg.audio.sample_rate);
        assert_eq!(again.tools.enabled, cfg.tools.enabled);
    }

    #[test]
    fn commands_model_must_be_absolute() {
        let mut cfg = example();
        cfg.commands.enabled = true;
        cfg.commands.model = "spotter_models/command".into();
        assert!(matches!(cfg.validate(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn tool_rounds_up_to_20_are_allowed() {
        let mut cfg = example();
        cfg.dialog.max_tool_rounds = 20;
        assert!(cfg.validate().is_ok());
        cfg.dialog.max_tool_rounds = 21;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn network_proxy_is_fallback_for_components() {
        let mut cfg = example();
        cfg.network.proxy = "socks5h://127.0.0.1:1080".into();
        cfg.stt.proxy.clear();
        cfg.llm.proxy = "http://127.0.0.1:8080".into();
        assert_eq!(cfg.proxy_for(&cfg.stt.proxy), cfg.network.proxy);
        assert_eq!(cfg.proxy_for(&cfg.llm.proxy), cfg.llm.proxy);
        cfg.network.proxy = "ftp://127.0.0.1:21".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn unknown_key_is_rejected() {
        let text = "[audio]\ncapure_pcm = \"default\"\n";
        assert!(toml::from_str::<Config>(text).is_err());
    }

    #[test]
    fn computer_config_requires_valid_mac_and_absolute_paths() {
        let mut cfg = example();
        cfg.tools.computer = Some(ComputerConfig {
            host: "192.168.3.2".into(),
            mac: "fc:34:97:17:d1:00".into(),
            broadcast: "192.168.3.255".into(),
            wake_port: 9,
            ssh_port: 2222,
            ssh_bin: "/data/petrovich/pc/dbclient".into(),
            key: "/data/petrovich/pc/client-key".into(),
            home: "/data/petrovich/pc".into(),
            timeout_ms: 12_000,
        });
        assert!(cfg.validate().is_ok());
        cfg.tools.computer.as_mut().unwrap().mac = "fc:34:97:17:d1".into();
        assert!(cfg.validate().is_err());
        cfg.tools.computer.as_mut().unwrap().mac = "fc:34:97:17:d1:00".into();
        cfg.tools.computer.as_mut().unwrap().key = "client-key".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn nightscout_requires_https() {
        let mut cfg = example();
        cfg.tools.nightscout_url = "http://example.com".into();
        assert!(cfg.validate().is_err());
        cfg.tools.nightscout_url = "https://example.com".into();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn plain_http_is_rejected_except_loopback() {
        let mut cfg = example();
        cfg.llm.base_url = "http://example.com/v1".into();
        assert!(cfg.validate().is_err());
        cfg.llm.base_url = "http://127.0.0.1:8080/v1".into();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn bad_channel_is_rejected() {
        let mut cfg = example();
        cfg.audio.mic_channels.push(99);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn command_provider_requires_argv() {
        let mut cfg = example();
        cfg.tts.provider = TtsProvider::Command;
        assert!(cfg.validate().is_err());
        cfg.tts.command = vec!["rhvoice-client".into(), "-v".into(), "anna".into()];
        assert!(cfg.validate().is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn secrets_must_not_be_world_readable() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.env");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "PETROVICH_LLM_API_KEY=test").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            Secrets::load(&path),
            Err(ConfigError::SecretPermissions { .. })
        ));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let s = Secrets::load(&path).unwrap();
        assert_eq!(s.get("PETROVICH_LLM_API_KEY"), Some("test"));
        assert!(!format!("{s:?}").contains("test"));
    }
}
