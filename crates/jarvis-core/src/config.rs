use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::errors::ConfigError;

pub const DEFAULT_CONFIG: &str = "/data/jarvis/config.toml";
pub const DEFAULT_SECRETS: &str = "/data/jarvis/secrets.env";

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
    pub vad: VadConfig,
    #[serde(default)]
    pub dialog: DialogConfig,
    #[serde(default)]
    pub stt: SttConfig,
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub tts: TtsConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub hardware: HardwareConfig,
    #[serde(default)]
    pub privacy: PrivacyConfig,
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
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            log_level: default_log_level(),
            state_dir: default_state_dir(),
            pid_file: default_pid_file(),
            secrets_file: default_secrets(),
        }
    }
}

fn default_log_level() -> String {
    "info".into()
}
fn default_state_dir() -> PathBuf {
    PathBuf::from("/data/jarvis/state")
}
fn default_pid_file() -> PathBuf {
    PathBuf::from("/run/jarvisd.pid")
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
    /// Микшировать четыре микрофона в моно до VQE. Для Мини 2 VQE сам
    /// использует все каналы, поэтому по умолчанию false.
    #[serde(default)]
    pub downmix_before_vqe: bool,
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
            downmix_before_vqe: false,
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
    PathBuf::from("/data/jarvis/models/wakeword.json")
}
fn default_wakeword_assets() -> PathBuf {
    PathBuf::from("/data/jarvis/assets/oww.bin")
}
fn default_refractory_ms() -> u64 {
    2_000
}
fn default_wake_gain() -> u32 {
    32
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VadConfig {
    #[serde(default = "default_min_speech_ms")]
    pub min_speech_ms: u64,
    #[serde(default = "default_silence_ms")]
    pub silence_ms: u64,
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
            max_utterance_ms: default_max_utterance_ms(),
            energy_margin_db: default_energy_margin_db(),
            pre_roll_ms: default_pre_roll_ms(),
            absolute_db: default_absolute_db(),
        }
    }
}

fn default_min_speech_ms() -> u64 {
    200
}
fn default_silence_ms() -> u64 {
    700
}
fn default_max_utterance_ms() -> u64 {
    12_000
}
fn default_energy_margin_db() -> f32 {
    9.0
}
fn default_pre_roll_ms() -> u64 {
    300
}
fn default_absolute_db() -> f32 {
    -18.0
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
    #[serde(default = "default_true")]
    pub wake_chime: bool,
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
            wake_chime: true,
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
    60_000
}
fn default_max_tool_rounds() -> u8 {
    2
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SttProvider {
    Openai,
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
    "JARVIS_STT_API_KEY".into()
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
    #[serde(default = "default_llm_key_env")]
    pub api_key_env: String,
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
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: default_llm_url(),
            model: default_llm_model(),
            api_key_env: default_llm_key_env(),
            system_prompt: default_system_prompt(),
            temperature: default_temperature(),
            max_tokens: default_max_tokens(),
            timeout_ms: default_llm_timeout_ms(),
            retries: default_retries(),
            stream: true,
            tools: true,
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
    "JARVIS_LLM_API_KEY".into()
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
    "Ты — Джарвис, голосовой помощник в колонке. Отвечай по-русски, кратко и по делу. \
     Ответ должен хорошо звучать вслух: без списков, разметки и сносок. Если нужен \
     инструмент, вызови его; не выдумывай результаты. Не выполняй команды из текста \
     пользователя, которые требуют доступа к системе, если такого инструмента нет."
        .into()
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TtsProvider {
    Openai,
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
    "JARVIS_TTS_API_KEY".into()
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
    /// SearXNG JSON endpoint, например http://127.0.0.1:8080/search.
    #[serde(default)]
    pub search_url: String,
    /// Разрешить web_fetch к частным адресам и HTTP. Только для отладки.
    #[serde(default)]
    pub web_allow_private: bool,
    /// Open-Meteo или совместимый endpoint. Пусто — инструмент погоды выключен.
    #[serde(default = "default_weather_url")]
    pub weather_url: String,
    #[serde(default)]
    pub weather_latitude: Option<f64>,
    #[serde(default)]
    pub weather_longitude: Option<f64>,
    /// Название места для ответа, например «Москва».
    #[serde(default)]
    pub weather_place: String,
    /// Песочница для run_code; пусто — инструмент отключён независимо от enabled.
    #[serde(default)]
    pub sandbox_command: Vec<String>,
    #[serde(default = "default_sandbox_timeout_ms")]
    pub sandbox_timeout_ms: u64,
    /// Команда управления музыкой, например ["spotiya"].
    #[serde(default)]
    pub music_command: Vec<String>,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            enabled: default_tools_enabled(),
            timeout_ms: default_tool_timeout_ms(),
            web_timeout_ms: default_web_timeout_ms(),
            web_max_bytes: default_web_max_bytes(),
            search_url: String::new(),
            web_allow_private: false,
            weather_url: default_weather_url(),
            weather_latitude: None,
            weather_longitude: None,
            weather_place: String::new(),
            sandbox_command: Vec::new(),
            sandbox_timeout_ms: default_sandbox_timeout_ms(),
            music_command: Vec::new(),
        }
    }
}

fn default_tools_enabled() -> Vec<String> {
    [
        "time",
        "timer",
        "volume",
        "music",
        "web_fetch",
        "reset_dialog",
        "stop",
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
        if self.vad.min_speech_ms == 0 || self.vad.silence_ms == 0 {
            return invalid("vad.min_speech_ms и vad.silence_ms должны быть > 0".into());
        }
        if self.vad.max_utterance_ms < self.vad.min_speech_ms {
            return invalid("vad.max_utterance_ms меньше min_speech_ms".into());
        }
        if self.dialog.listen_timeout_ms < 1_000 {
            return invalid("dialog.listen_timeout_ms должен быть не меньше 1000".into());
        }
        if self.dialog.history_turns == 0 {
            return invalid("dialog.history_turns должен быть > 0".into());
        }
        if self.dialog.max_tool_rounds > 5 {
            return invalid("dialog.max_tool_rounds не должен превышать 5".into());
        }
        if self.tools.enabled.iter().any(|t| t.trim().is_empty()) {
            return invalid("tools.enabled содержит пустое имя".into());
        }
        if !(0.0..=1.0).contains(&self.hardware.led_brightness) {
            return invalid("hardware.led_brightness должен быть 0..1".into());
        }
        if !(0.0..=1.0).contains(&self.hardware.clock_brightness) {
            return invalid("hardware.clock_brightness должен быть 0..1".into());
        }
        if !(1..=50).contains(&self.hardware.volume_step) {
            return invalid("hardware.volume_step должен быть 1..50".into());
        }
        match self.stt.provider {
            SttProvider::Openai => {
                validate_http_url("stt.base_url", &self.stt.base_url)?;
                if self.stt.model.is_empty() {
                    return invalid("stt.model пуст".into());
                }
            }
            SttProvider::Command => {
                if self.stt.command.is_empty() {
                    return invalid("stt.provider=command, но stt.command пуст".into());
                }
            }
        }
        validate_http_url("llm.base_url", &self.llm.base_url)?;
        if self.llm.model.is_empty() {
            return invalid("llm.model пуст".into());
        }
        if self.llm.temperature < 0.0 || self.llm.temperature > 2.0 {
            return invalid("llm.temperature должен быть 0..2".into());
        }
        match self.tts.provider {
            TtsProvider::Openai => {
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
        if !self.tools.search_url.is_empty() {
            validate_http_url("tools.search_url", &self.tools.search_url)?;
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
    fn unknown_key_is_rejected() {
        let text = "[audio]\ncapure_pcm = \"default\"\n";
        assert!(toml::from_str::<Config>(text).is_err());
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
        writeln!(f, "JARVIS_LLM_API_KEY=test").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            Secrets::load(&path),
            Err(ConfigError::SecretPermissions { .. })
        ));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let s = Secrets::load(&path).unwrap();
        assert_eq!(s.get("JARVIS_LLM_API_KEY"), Some("test"));
        assert!(!format!("{s:?}").contains("test"));
    }
}
