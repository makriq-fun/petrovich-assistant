use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("ошибка ввода-вывода: {0}")]
    Io(#[from] std::io::Error),
    #[error("ошибка разбора: {0}")]
    Parse(String),
    #[error("неверная конфигурация: {0}")]
    Config(String),
    #[error("внешний сервис: {0}")]
    External(String),
    #[error("тайм-аут: {0}")]
    Timeout(String),
    #[error("операция запрещена: {0}")]
    Forbidden(String),
    #[error("не поддержано: {0}")]
    Unsupported(String),
    #[error("аудио: {0}")]
    Audio(String),
    #[error("оборудование: {0}")]
    Hardware(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("не удалось прочитать {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("не удалось разобрать {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("неверная конфигурация: {0}")]
    Invalid(String),
    #[error("файл секретов {path} имеет права {mode:o}, нужны 0600 или 0400")]
    SecretPermissions { path: PathBuf, mode: u32 },
    #[error("файл секретов {path}: {source}")]
    SecretRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("секрет {env} не задан в {path}")]
    MissingSecret { env: String, path: PathBuf },
}
