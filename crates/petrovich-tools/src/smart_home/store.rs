//! Приватное хранилище сессии и локальных ключей.
//!
//! Файлы лежат в `<state_dir>/smart_home`, каталог 0700, файлы 0600. Запись
//! атомарная: временный файл, fsync, rename. Содержимое никогда не пишется
//! в журнал.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::cloud::Session;
use super::model::DeviceRecord;

#[derive(Clone, Debug)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn session_path(&self) -> PathBuf {
        self.dir.join("session.json")
    }

    fn devices_path(&self) -> PathBuf {
        self.dir.join("devices.json")
    }

    pub fn load_session(&self) -> Result<Option<Session>> {
        read_private_json(&self.session_path())
    }

    pub fn save_session(&self, session: &Session) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(session).context("сериализация сессии Tuya")?;
        write_private(&self.session_path(), &bytes)
    }

    pub fn load_devices(&self) -> Result<Vec<DeviceRecord>> {
        Ok(read_private_json::<Vec<DeviceRecord>>(&self.devices_path())?.unwrap_or_default())
    }

    pub fn save_devices(&self, devices: &[DeviceRecord]) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(devices).context("сериализация устройств Tuya")?;
        write_private(&self.devices_path(), &bytes)
    }

    pub fn clear(&self) -> Result<()> {
        for path in [self.session_path(), self.devices_path()] {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("удаление {}", path.display())),
            }
        }
        Ok(())
    }
}

fn read_private_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("чтение {}", path.display())),
    };
    let mode = fs::metadata(path)
        .with_context(|| format!("права {}", path.display()))?
        .permissions()
        .mode()
        & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "{} имеет права {mode:o}; нужны 0600. Выполните: chmod 600 {}",
            path.display(),
            path.display()
        );
    }
    let value =
        serde_json::from_slice(&data).with_context(|| format!("разбор {}", path.display()))?;
    Ok(Some(value))
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("путь без каталога: {}", path.display()))?;
    fs::create_dir_all(dir).with_context(|| format!("каталог {}", dir.display()))?;
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    let tmp = dir.join(format!(".tmp-{}-{}", std::process::id(), rand_suffix()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("создание {}", tmp.display()))?;
        file.write_all(data)
            .with_context(|| format!("запись {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("fsync {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("переименование {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("права {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn rand_suffix() -> u64 {
    use rand::RngCore;
    rand::thread_rng().next_u64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smart_home::cloud::{TokenInfo, HA_CLIENT_ID};
    use std::os::unix::fs::PermissionsExt;

    fn session() -> Session {
        Session {
            client_id: HA_CLIENT_ID.into(),
            user_code: "user".into(),
            terminal_id: "term".into(),
            endpoint: "https://apigw.iotbing.com".into(),
            token_info: TokenInfo {
                t: 1,
                uid: "u".into(),
                expire_time: 3600,
                access_token: "ACCESS-SECRET".into(),
                refresh_token: "REFRESH-SECRET".into(),
            },
        }
    }

    #[test]
    fn roundtrip_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("smart_home"));
        assert!(store.load_session().unwrap().is_none());
        store.save_session(&session()).unwrap();
        let loaded = store.load_session().unwrap().unwrap();
        assert_eq!(loaded.token_info.access_token, "ACCESS-SECRET");
        let mode = fs::metadata(store.session_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = fs::metadata(store.dir()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
    }

    #[test]
    fn rejects_world_readable_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.save_session(&session()).unwrap();
        fs::set_permissions(store.session_path(), fs::Permissions::from_mode(0o644)).unwrap();
        let err = store.load_session().unwrap_err().to_string();
        assert!(err.contains("0600"), "{err}");
    }

    #[test]
    fn devices_roundtrip_without_leaking_key_in_debug() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let record = DeviceRecord {
            id: "id".into(),
            name: "Лампа".into(),
            category: "dj".into(),
            product_id: String::new(),
            product_name: String::new(),
            model: String::new(),
            local_key: "0123456789abcdef".into(),
            sub: false,
            gateway_id: None,
            node_id: None,
            online: true,
            room: None,
            home_id: String::new(),
            home_name: String::new(),
            functions: Vec::new(),
            status: Vec::new(),
            dp_map: Default::default(),
            cloud_status: Default::default(),
            local_supported: true,
            unsupported_reason: String::new(),
            ip: Some("192.168.1.2".into()),
            version: None,
            device22: false,
            last_seen: 0,
            key_from: None,
        };
        store.save_devices(std::slice::from_ref(&record)).unwrap();
        let loaded = store.load_devices().unwrap();
        assert_eq!(loaded[0].local_key, "0123456789abcdef");
        let debug = format!("{:?}", loaded[0]);
        assert!(!debug.contains("0123456789abcdef"));
        assert!(!debug.contains("id"));
    }

    #[test]
    fn clear_removes_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.save_session(&session()).unwrap();
        store.clear().unwrap();
        assert!(store.load_session().unwrap().is_none());
        assert!(!store.session_path().exists());
        assert!(!store.devices_path().exists());
    }
}
