//! Умный дом Tuya: QR-вход, локальный протокол и инструмент `smart_home`.
//!
//! Управление идёт только по локальной сети. Облако используется один раз
//! при входе и при обновлении ключей. LLM видит названия устройств, но не
//! идентификаторы, ключи или произвольные DP.

pub mod catalog;
pub mod cloud;
pub mod lan;
pub mod model;
pub mod resolve;
pub mod store;
#[cfg(test)]
pub(crate) mod testutil;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use futures_util::stream::{self, StreamExt};
use petrovich_core::chat::ToolDefinition;
use petrovich_core::config::{parse_ipv4_subnet, SmartHomeConfig};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::smart_home::cloud::CloudClient;
pub use crate::smart_home::model::{CategoryPolicy, DeviceKind, DeviceRecord, ProtocolVersion};
use crate::smart_home::resolve::{select, SelectMode, Selection};

#[derive(Clone)]
pub struct SmartHome {
    inner: Arc<Inner>,
}

struct Inner {
    store: store::Store,
    cfg: SmartHomeConfig,
    records: RwLock<Vec<DeviceRecord>>,
    scan_gate: tokio::sync::Mutex<()>,
    last_scan: Mutex<HashMap<String, Instant>>,
    proxy: String,
}

pub struct LoginSummary {
    pub devices: usize,
    pub allowed: usize,
    pub warning: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    action: Action,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    room: Option<String>,
    #[serde(default)]
    power: Option<bool>,
    #[serde(default)]
    brightness: Option<u8>,
    #[serde(default)]
    brightness_delta: Option<i8>,
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    color_temp: Option<u8>,
    #[serde(default)]
    color_temp_delta: Option<i8>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    temperature_delta: Option<f32>,
    #[serde(default)]
    fan_speed: Option<String>,
    #[serde(default)]
    position: Option<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Action {
    List,
    State,
    Set,
    Adjust,
}

#[derive(Clone, Debug)]
struct Outcome {
    name: String,
    room: String,
    ok: bool,
    verified: bool,
    message: String,
}

impl SmartHome {
    /// Загружает сессию и устройства из state_dir. Сеть не нужна.
    pub async fn open(
        cfg: SmartHomeConfig,
        state_dir: impl AsRef<Path>,
        proxy: &str,
    ) -> Result<Self> {
        let store = store::Store::new(state_dir.as_ref().join("smart_home"));
        let session = store.load_session()?.ok_or_else(|| {
            anyhow::anyhow!("нет сессии Tuya; выполните petrovichctl smart-home login")
        })?;
        if session.token_info.access_token.is_empty() || session.token_info.refresh_token.is_empty()
        {
            bail!("сессия Tuya повреждена; выполните вход заново");
        }
        let records = store.load_devices()?;
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                cfg,
                records: RwLock::new(records),
                scan_gate: tokio::sync::Mutex::new(()),
                last_scan: Mutex::new(HashMap::new()),
                proxy: proxy.trim().to_owned(),
            }),
        })
    }

    pub fn session_present(state_dir: impl AsRef<Path>) -> bool {
        store::Store::new(state_dir.as_ref().join("smart_home"))
            .load_session()
            .ok()
            .flatten()
            .is_some()
    }

    /// QR-вход. После подтверждения сразу забирает устройства и ключи.
    pub async fn login(
        cfg: &SmartHomeConfig,
        state_dir: impl AsRef<Path>,
        user_code: &str,
        scheme: &str,
        qr: impl FnMut(&str),
        proxy: &str,
    ) -> Result<LoginSummary> {
        let store = store::Store::new(state_dir.as_ref().join("smart_home"));
        let login = CloudClient::login(
            user_code,
            scheme,
            cfg.qr_timeout_s,
            cfg.cloud_timeout_ms,
            proxy,
            qr,
        )
        .await?;
        store.save_session(&login.session)?;
        let mut client = CloudClient::new(login.session.clone(), cfg.cloud_timeout_ms, proxy)?;
        match client.fetch_devices().await {
            Ok(mut devices) => {
                store.save_session(client.session())?;
                merge_discovery(&store, &mut devices);
                let devices_len = devices.len();
                let allowed = devices
                    .iter()
                    .filter(|d| {
                        matches!(
                            catalog::policy_for(&d.category),
                            CategoryPolicy::Control(_) | CategoryPolicy::ReadOnly(_)
                        )
                    })
                    .count();
                store.save_devices(&devices)?;
                Ok(LoginSummary {
                    devices: devices_len,
                    allowed,
                    warning: None,
                })
            }
            Err(e) => Ok(LoginSummary {
                devices: 0,
                allowed: 0,
                warning: Some(format!(
                    "вход выполнен, но список устройств не получен: {e}. Повторите: petrovichctl smart-home refresh"
                )),
            }),
        }
    }

    pub fn logout(state_dir: impl AsRef<Path>) -> Result<()> {
        store::Store::new(state_dir.as_ref().join("smart_home")).clear()
    }

    /// Перечитывает ключи и комнаты из облака. Локальные IP/версии
    /// сохраняются, чтобы не сканировать сеть заново.
    pub async fn refresh_cloud(&self) -> Result<usize> {
        let session = self
            .inner
            .store
            .load_session()?
            .ok_or_else(|| anyhow::anyhow!("нет сессии Tuya"))?;
        let mut client =
            CloudClient::new(session, self.inner.cfg.cloud_timeout_ms, &self.inner.proxy)?;
        let mut devices = client.fetch_devices().await?;
        self.inner.store.save_session(client.session())?;
        merge_discovery(&self.inner.store, &mut devices);
        let count = devices.len();
        self.inner.store.save_devices(&devices)?;
        *self
            .inner
            .records
            .write()
            .map_err(|_| anyhow::anyhow!("каталог повреждён"))? = devices;
        Ok(count)
    }

    /// Определение инструмента. В описании только имена и комнаты, без
    /// идентификаторов и ключей. Схема не содержит device_id и raw DP.
    pub fn definition(&self) -> ToolDefinition {
        let records = self.inner.records.read().ok();
        let mut devices = Vec::new();
        let mut rooms = Vec::new();
        if let Some(records) = records.as_deref() {
            for record in records {
                if !is_visible(record) {
                    continue;
                }
                let name = catalog::display_name(record);
                let kind = kind_of(record);
                devices.push(format!("{name} ({})", kind.label()));
                if let Some(room) = &record.room {
                    if !room.name.is_empty() {
                        rooms.push(room.name.clone());
                    }
                }
            }
        }
        rooms.sort();
        rooms.dedup();
        let summary = if devices.is_empty() {
            "Сейчас нет разрешённых устройств.".to_owned()
        } else {
            format!(
                "Устройства: {}. Комнаты: {}.",
                truncate(&devices.join(", "), 900),
                if rooms.is_empty() {
                    "не указаны".to_owned()
                } else {
                    truncate(&rooms.join(", "), 200)
                }
            )
        };
        let description = format!(
            "Управление умным домом Tuya по локальной сети. {summary} \
             action=list — список; state — состояние; set — включить/выключить, яркость, цвет, \
             температура, режим; adjust — изменить на delta (теплее/холоднее, ярче/тусклее). \
             target — название или группа: «весь свет», «розетки», «лампа»; room — комната. \
             Неизвестные, запрещённые и критичные устройства недоступны."
        );
        ToolDefinition::new(
            "smart_home",
            &description,
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "state", "set", "adjust"]},
                    "target": {"type": "string", "maxLength": 120},
                    "room": {"type": "string", "maxLength": 80},
                    "power": {"type": "boolean"},
                    "brightness": {"type": "integer", "minimum": 0, "maximum": 100},
                    "brightness_delta": {"type": "integer", "minimum": -100, "maximum": 100},
                    "color": {"type": "string", "pattern": "^#[0-9A-Fa-f]{6}$"},
                    "color_temp": {"type": "integer", "minimum": 0, "maximum": 100},
                    "color_temp_delta": {"type": "integer", "minimum": -100, "maximum": 100},
                    "mode": {"type": "string", "maxLength": 40},
                    "temperature": {"type": "number", "minimum": 5, "maximum": 35},
                    "temperature_delta": {"type": "number", "minimum": -10, "maximum": 10},
                    "fan_speed": {"type": "string", "maxLength": 40},
                    "position": {"type": "integer", "minimum": 0, "maximum": 100}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        )
    }

    pub fn has_allowed_devices(&self) -> bool {
        self.inner
            .records
            .read()
            .map(|r| r.iter().any(is_visible))
            .unwrap_or(false)
    }

    pub async fn execute(&self, args: &Value) -> Result<String> {
        let request: Request =
            serde_json::from_value(args.clone()).context("smart_home: неверные аргументы")?;
        validate_request(&request)?;
        match request.action {
            Action::List => Ok(self.list_text()),
            Action::State => self.state_text(&request).await,
            Action::Set | Action::Adjust => self.apply_text(&request).await,
        }
    }

    fn list_text(&self) -> String {
        let Ok(records) = self.inner.records.read() else {
            return "Каталог устройств недоступен.".into();
        };
        let mut lines = Vec::new();
        for record in records.iter() {
            if !is_visible(record) {
                continue;
            }
            let kind = kind_of(record);
            let caps = catalog::capabilities(record);
            let features = match &caps {
                Ok(caps) if caps.has_control() => caps.feature_labels().join(", "),
                Ok(_) => "только чтение".to_owned(),
                Err(reason) => format!("локально недоступно: {reason}"),
            };
            lines.push(format!(
                "- {} — {}, {}, {}",
                catalog::display_name(record),
                kind.label(),
                if record.online {
                    "онлайн"
                } else {
                    "офлайн"
                },
                features
            ));
        }
        let hidden = records.iter().filter(|r| !is_visible(r)).count();
        drop(records);
        if lines.is_empty() {
            return "Нет разрешённых устройств: политика безопасности не нашла ни одного света, розетки, выключателя или климатического устройства.".into();
        }
        let mut text = format!(
            "Разрешённые устройства ({}):\n{}",
            lines.len(),
            lines.join("\n")
        );
        if hidden > 0 {
            text.push_str(&format!("\nСкрыто политикой безопасности: {hidden}."));
        }
        text
    }

    async fn state_text(&self, request: &Request) -> Result<String> {
        let records = {
            let guard = self
                .inner
                .records
                .read()
                .map_err(|_| anyhow::anyhow!("каталог повреждён"))?;
            guard.clone()
        };
        let policies = policies_for(&records);
        let selection = self.select(&records, &policies, request, SelectMode::State)?;
        if let Some(clarification) = selection.clarification {
            return Ok(clarification);
        }
        if selection.indices.is_empty() {
            return Ok("Нет устройств для запроса.".into());
        }
        let limit = self.inner.cfg.max_parallel.max(1);
        let items: Vec<(usize, DeviceRecord)> = selection
            .indices
            .into_iter()
            .map(|i| (i, records[i].clone()))
            .collect();
        let mut results: Vec<(usize, Result<String>)> = stream::iter(items)
            .map(|(index, record)| async move {
                let result = self.query_text(record).await;
                (index, result)
            })
            .buffer_unordered(limit)
            .collect()
            .await;
        results.sort_by_key(|(i, _)| *i);
        let mut lines = Vec::new();
        for (index, result) in results {
            match result {
                Ok(text) => lines.push(text),
                Err(e) => lines.push(format!(
                    "{}: ошибка — {e}",
                    catalog::display_name(&records[index])
                )),
            }
        }
        Ok(lines.join("\n"))
    }

    async fn apply_text(&self, request: &Request) -> Result<String> {
        if request
            .target
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
            && request
                .room
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .is_empty()
        {
            bail!("не указано устройство или комната");
        }
        let records = {
            let guard = self
                .inner
                .records
                .read()
                .map_err(|_| anyhow::anyhow!("каталог повреждён"))?;
            guard.clone()
        };
        let policies = policies_for(&records);
        let selection = self.select(&records, &policies, request, SelectMode::Control)?;
        if let Some(clarification) = selection.clarification {
            return Ok(clarification);
        }
        if selection.indices.is_empty() {
            bail!("не выбрано ни одного устройства");
        }
        let limit = self.inner.cfg.max_parallel.max(1);
        let items: Vec<(usize, DeviceRecord)> = selection
            .indices
            .into_iter()
            .map(|i| (i, records[i].clone()))
            .collect();
        let mut outcomes: Vec<(usize, Outcome)> = stream::iter(items)
            .map(|(index, record)| {
                let request = request.clone();
                async move {
                    let outcome = self.apply_device(record, &request).await;
                    (index, outcome)
                }
            })
            .buffer_unordered(limit)
            .collect()
            .await;
        outcomes.sort_by_key(|(i, _)| *i);
        let mut success = Vec::new();
        let mut failed = Vec::new();
        for (_, outcome) in outcomes {
            let label = if outcome.room.is_empty() {
                outcome.name
            } else {
                format!("{} ({})", outcome.name, outcome.room)
            };
            if outcome.ok {
                if outcome.verified {
                    success.push(format!("{label}: {}", outcome.message));
                } else {
                    failed.push(format!("{label}: {}", outcome.message));
                }
            } else {
                failed.push(format!("{label}: {}", outcome.message));
            }
        }
        let mut text = String::new();
        if !success.is_empty() {
            text.push_str("Готово. ");
            text.push_str(&success.join("; "));
            text.push('.');
        }
        if !failed.is_empty() {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str("Не удалось: ");
            text.push_str(&failed.join("; "));
            text.push('.');
        }
        if success.is_empty() && failed.is_empty() {
            bail!("команда не выполнена");
        }
        if success.is_empty() {
            bail!("{text}");
        }
        Ok(text)
    }

    async fn query_text(&self, mut record: DeviceRecord) -> Result<String> {
        let result = self.query_with_recovery(&mut record).await?;
        let state = catalog::decode_state(&record, &result.dps);
        Ok(catalog::describe_state(&record, &state))
    }

    async fn apply_device(&self, mut record: DeviceRecord, request: &Request) -> Outcome {
        let name = record.name.clone();
        let room = record
            .room
            .as_ref()
            .map(|r| r.name.clone())
            .unwrap_or_default();
        let mut outcome = Outcome {
            name,
            room,
            ok: false,
            verified: false,
            message: String::new(),
        };
        let result = async {
            self.ensure_ready(&mut record).await?;
            let needs_current = request.has_deltas();
            let current = if needs_current {
                Some(
                    self.query_with_recovery(&mut record)
                        .await
                        .map(|r| catalog::decode_state(&record, &r.dps))?,
                )
            } else {
                None
            };
            let changes = request.to_changes(current.as_ref())?;
            let caps = catalog::capabilities(&record).map_err(anyhow::Error::msg)?;
            let plan = catalog::plan(&record, &caps, &changes).map_err(anyhow::Error::msg)?;
            if plan.is_empty() {
                bail!("нечего менять");
            }
            let acknowledged = match lan::control_record(&record, &plan, self.timeout()).await {
                Ok(ack) => ack.acknowledged,
                Err(e) => {
                    // Устройство могло сменить IP или перезагрузиться.
                    let fresh = self
                        .discover(&record, true)
                        .await
                        .context("повторный поиск после ошибки")?;
                    let caps = catalog::capabilities(&fresh).map_err(anyhow::Error::msg)?;
                    let plan =
                        catalog::plan(&fresh, &caps, &changes).map_err(anyhow::Error::msg)?;
                    let ack = lan::control_record(&fresh, &plan, self.timeout())
                        .await
                        .with_context(|| format!("команда не прошла: {e}"))?;
                    record = fresh;
                    ack.acknowledged
                }
            };
            let state = match self.query_with_recovery(&mut record).await {
                Ok(reply) => Some(catalog::decode_state(&record, &reply.dps)),
                Err(_) => None,
            };
            let (verified, detail) = match &state {
                Some(state) => verify(&changes, state),
                None if acknowledged => (
                    false,
                    "команда отправлена, состояние не подтвердилось".to_owned(),
                ),
                None => {
                    bail!("устройство не подтвердило команду и не ответило на запрос состояния")
                }
            };
            Ok::<_, anyhow::Error>((verified, detail, state))
        }
        .await;
        match result {
            Ok((verified, detail, state)) => {
                outcome.ok = true;
                outcome.verified = verified;
                outcome.message = if verified {
                    detail
                } else if let Some(state) = state {
                    format!(
                        "{detail}; устройство сообщает: {}",
                        catalog::describe_state(&record, &state)
                    )
                } else {
                    detail
                };
            }
            Err(e) => {
                outcome.message = e.to_string();
            }
        }
        outcome
    }

    fn select(
        &self,
        records: &[DeviceRecord],
        policies: &[CategoryPolicy],
        request: &Request,
        mode: SelectMode,
    ) -> Result<Selection> {
        select(
            records,
            policies,
            &self.inner.cfg,
            request.target.as_deref(),
            request.room.as_deref(),
            mode,
        )
        .map_err(anyhow::Error::msg)
    }

    fn timeout(&self) -> Duration {
        Duration::from_millis(self.inner.cfg.timeout_ms)
    }

    async fn ensure_ready(&self, record: &mut DeviceRecord) -> Result<()> {
        if record.ip.is_none() || record.version.is_none() {
            *record = self.discover(record, false).await?;
        }
        Ok(())
    }

    async fn query_with_recovery(&self, record: &mut DeviceRecord) -> Result<lan::QueryResult> {
        self.ensure_ready(record).await?;
        match lan::query_record(record, self.timeout()).await {
            Ok(result) => {
                self.remember_discovery(record, result.device22);
                self.touch(&record.id);
                Ok(result)
            }
            Err(_) => {
                let fresh = self.discover(record, false).await?;
                *record = fresh;
                let result = lan::query_record(record, self.timeout()).await?;
                self.remember_discovery(record, result.device22);
                self.touch(&record.id);
                Ok(result)
            }
        }
    }

    async fn discover(&self, record: &DeviceRecord, force: bool) -> Result<DeviceRecord> {
        // Дочернее Zigbee-устройство ходит через шлюз: сначала находим шлюз.
        if record.sub {
            if let Some(gateway_id) = record.gateway_id.as_deref() {
                let gateway = self
                    .inner
                    .records
                    .read()
                    .ok()
                    .and_then(|records| records.iter().find(|r| r.id == gateway_id).cloned());
                if let Some(gateway) = gateway {
                    let gateway = if gateway.ip.is_none() || gateway.version.is_none() {
                        Box::pin(self.discover(&gateway, force)).await?
                    } else {
                        gateway
                    };
                    if let Some(ip) = gateway
                        .ip
                        .as_deref()
                        .and_then(|ip| ip.parse::<std::net::IpAddr>().ok())
                    {
                        let mut candidate = record.clone();
                        candidate.ip = Some(ip.to_string());
                        candidate.version = gateway.version;
                        if candidate.node_id.is_none() {
                            candidate.node_id = Some(candidate.id.clone());
                        }
                        if let Ok((version, reply)) =
                            lan::probe_at(&candidate, ip, &self.discovery_config(), gateway.version)
                                .await
                        {
                            let mut found = record.clone();
                            found.ip = Some(ip.to_string());
                            found.version = Some(version);
                            found.device22 = reply.device22;
                            found.last_seen = now();
                            self.store_discovered(&found);
                            return Ok(found);
                        }
                    }
                }
            }
            if record.ip.is_none() {
                bail!("шлюз Zigbee-устройства не найден; включите шлюз и повторите поиск");
            }
        }
        self.scan_record(record, force).await
    }

    async fn scan_record(&self, record: &DeviceRecord, force: bool) -> Result<DeviceRecord> {
        let found = self.scan_records(std::slice::from_ref(record), force).await;
        found
            .into_iter()
            .find(|r| r.id == record.id)
            .ok_or_else(|| anyhow::anyhow!("устройство не найдено в локальной сети"))
    }

    /// Ищет несколько устройств за один проход по подсети: TCP-порт
    /// проверяется один раз, а не для каждого устройства отдельно.
    async fn scan_records(&self, records: &[DeviceRecord], force: bool) -> Vec<DeviceRecord> {
        let _guard = self.inner.scan_gate.lock().await;
        let config = self.discovery_config();
        let mut found = Vec::new();
        let mut pending = Vec::new();
        let mut known = Vec::new();
        for record in records {
            let allowed = self.scan_allowed(&record.id);
            if !force && !allowed {
                continue;
            }
            if let Some(ip) = record.ip.as_deref().and_then(|ip| ip.parse().ok()) {
                known.push((record.clone(), ip));
            } else {
                pending.push(record.clone());
            }
        }
        let limit = self.inner.cfg.max_parallel.max(1);
        let probed: Vec<_> = stream::iter(known)
            .map(|(record, ip)| {
                let config = config.clone();
                async move {
                    let result = lan::probe_at(&record, ip, &config, record.version).await;
                    (record, ip, result)
                }
            })
            .buffer_unordered(limit)
            .collect()
            .await;
        for (record, ip, result) in probed {
            if let Ok((version, reply)) = result {
                let mut device = record;
                device.ip = Some(ip.to_string());
                device.version = Some(version);
                device.device22 = reply.device22;
                device.last_seen = now();
                self.store_discovered(&device);
                found.push(device);
            } else {
                pending.push(record);
            }
        }
        if pending.is_empty() {
            return found;
        }
        let Ok((base, prefix)) = self.subnet() else {
            return found;
        };
        let hosts = lan::hosts_in_subnet(base, prefix, self.inner.cfg.max_scan_hosts);
        let open = self.open_hosts(hosts).await;
        // Проверяем пары «устройство × открытый хост» параллельно. Раньше
        // вложенный последовательный перебор с тайм-аутом на каждую версию
        // протокола мог занимать десятки минут.
        let attempts = pending
            .into_iter()
            .flat_map(|record| open.iter().map(move |ip| (record.clone(), *ip)))
            .collect::<Vec<_>>();
        let limit = self.inner.cfg.max_parallel.max(1);
        let probed: Vec<_> = stream::iter(attempts)
            .map(|(record, ip)| {
                let config = config.clone();
                async move {
                    let result = lan::probe_at(&record, ip.into(), &config, record.version).await;
                    (record, ip, result)
                }
            })
            .buffer_unordered(limit)
            .collect()
            .await;
        for (record, ip, result) in probed {
            let Ok((version, reply)) = result else {
                continue;
            };
            if found.iter().any(|r| r.id == record.id) {
                continue;
            }
            let mut device = record;
            device.ip = Some(ip.to_string());
            device.version = Some(version);
            device.device22 = reply.device22;
            device.last_seen = now();
            self.store_discovered(&device);
            found.push(device);
        }
        found
    }

    async fn open_hosts(&self, hosts: Vec<std::net::Ipv4Addr>) -> Vec<std::net::Ipv4Addr> {
        let limit = self.inner.cfg.max_parallel.max(1);
        let timeout = Duration::from_millis(self.inner.cfg.timeout_ms.min(1500));
        stream::iter(hosts)
            .map(|ip| async move {
                if lan::port_open(ip, timeout).await {
                    Some(ip)
                } else {
                    None
                }
            })
            .buffer_unordered(limit)
            .filter_map(|v| async move { v })
            .collect()
            .await
    }

    fn subnet(&self) -> Result<(std::net::Ipv4Addr, u8)> {
        if !self.inner.cfg.scan_subnet.trim().is_empty() {
            parse_ipv4_subnet(&self.inner.cfg.scan_subnet)
                .ok_or_else(|| anyhow::anyhow!("неверная подсеть поиска"))
        } else {
            lan::local_subnet()
                .ok_or_else(|| anyhow::anyhow!("не удалось определить локальную подсеть"))
        }
    }

    fn discovery_config(&self) -> lan::DiscoveryConfig {
        lan::DiscoveryConfig {
            timeout: self.timeout(),
            scan_subnet: self.subnet().ok(),
            max_hosts: self.inner.cfg.max_scan_hosts,
            max_parallel: self.inner.cfg.max_parallel,
            allow_v31: self.inner.cfg.allow_v31,
        }
    }

    fn scan_allowed(&self, id: &str) -> bool {
        let Ok(mut map) = self.inner.last_scan.lock() else {
            return true;
        };
        let now = Instant::now();
        if let Some(last) = map.get(id) {
            if now.duration_since(*last) < Duration::from_secs(30) {
                return false;
            }
        }
        map.insert(id.to_owned(), now);
        if map.len() > 512 {
            map.retain(|_, t| now.duration_since(*t) < Duration::from_secs(600));
        }
        true
    }

    fn remember_discovery(&self, record: &DeviceRecord, device22: bool) {
        if record.device22 == device22 {
            return;
        }
        let mut device = record.clone();
        device.device22 = device22;
        self.store_discovered(&device);
    }

    /// Отмечает успешный локальный опрос, не переписывая файл на каждый
    /// запрос (флеш колонки не любит лишние записи).
    fn touch(&self, id: &str) {
        if let Ok(mut records) = self.inner.records.write() {
            if let Some(slot) = records.iter_mut().find(|r| r.id == id) {
                slot.last_seen = now();
            }
        }
    }

    fn store_discovered(&self, record: &DeviceRecord) {
        let Ok(mut records) = self.inner.records.write() else {
            return;
        };
        if let Some(slot) = records.iter_mut().find(|r| r.id == record.id) {
            slot.ip = record.ip.clone();
            slot.version = record.version;
            slot.device22 = record.device22;
            slot.last_seen = record.last_seen;
        }
        let snapshot = records.clone();
        drop(records);
        if let Err(e) = self.inner.store.save_devices(&snapshot) {
            tracing::warn!("умный дом: не удалось сохранить кэш устройств: {e}");
        }
    }

    /// Фоновый поиск только пропавших устройств. Не запускает облако.
    pub fn spawn_discovery(self: &Arc<Self>) {
        let interval = self.inner.cfg.scan_interval_s;
        if interval == 0 {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(interval.max(30)));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                let missing: Vec<DeviceRecord> = inner
                    .records
                    .read()
                    .map(|records| {
                        records
                            .iter()
                            .filter(|r| {
                                is_visible(r)
                                    && (r.ip.is_none()
                                        || r.version.is_none()
                                        || now() - r.last_seen > interval as i64 * 2)
                            })
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                if missing.is_empty() {
                    continue;
                }
                let home = SmartHome {
                    inner: inner.clone(),
                };
                let (subs, plain): (Vec<_>, Vec<_>) =
                    missing.into_iter().partition(|record| record.sub);
                for record in subs {
                    if let Err(e) = home.discover(&record, false).await {
                        tracing::debug!(
                            "умный дом: {} не найден: {e}",
                            catalog::display_name(&record)
                        );
                    }
                }
                let found = home.scan_records(&plain, false).await;
                for record in plain {
                    if !found.iter().any(|r| r.id == record.id) {
                        tracing::debug!("умный дом: {} не найден", catalog::display_name(&record));
                    }
                }
            }
        });
    }

    /// Принудительный поиск всех разрешённых устройств без IP/версии.
    pub async fn discover_missing(&self) -> usize {
        let missing: Vec<DeviceRecord> = self
            .inner
            .records
            .read()
            .map(|records| {
                records
                    .iter()
                    .filter(|r| {
                        (is_visible(r) || is_hub(r)) && (r.ip.is_none() || r.version.is_none())
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let (subs, plain): (Vec<_>, Vec<_>) = missing.into_iter().partition(|record| record.sub);
        let mut found = 0;
        for record in subs {
            if self.discover(&record, false).await.is_ok() {
                found += 1;
            }
        }
        found += self.scan_records(&plain, false).await.len();
        found
    }
}

fn merge_discovery(store: &store::Store, devices: &mut [DeviceRecord]) {
    let Ok(old) = store.load_devices() else {
        return;
    };
    let old: BTreeMap<String, DeviceRecord> = old.into_iter().map(|r| (r.id.clone(), r)).collect();
    for device in devices {
        if let Some(previous) = old.get(&device.id) {
            device.ip = previous.ip.clone().or(device.ip.take());
            device.version = previous.version.or(device.version.take());
            device.device22 = previous.device22;
            device.last_seen = previous.last_seen;
            if device.room.is_none() {
                device.room = previous.room.clone();
            }
        }
    }
}

fn policies_for(records: &[DeviceRecord]) -> Vec<CategoryPolicy> {
    records
        .iter()
        .map(|r| catalog::policy_for(&r.category))
        .collect()
}

fn is_hub(record: &DeviceRecord) -> bool {
    matches!(
        catalog::policy_for(&record.category),
        CategoryPolicy::ReadOnly(DeviceKind::Hub)
    )
}

fn is_visible(record: &DeviceRecord) -> bool {
    matches!(
        catalog::policy_for(&record.category),
        CategoryPolicy::Control(_) | CategoryPolicy::ReadOnly(_)
    ) && !is_hub(record)
}

fn kind_of(record: &DeviceRecord) -> DeviceKind {
    match catalog::policy_for(&record.category) {
        CategoryPolicy::Control(k) | CategoryPolicy::ReadOnly(k) => k,
        _ => DeviceKind::Hub,
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_owned();
    }
    let mut out: String = value.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

impl Request {
    fn has_deltas(&self) -> bool {
        self.brightness_delta.is_some()
            || self.color_temp_delta.is_some()
            || self.temperature_delta.is_some()
    }

    fn to_changes(&self, current: Option<&catalog::DeviceState>) -> Result<catalog::ChangeSet> {
        let mut changes = catalog::ChangeSet {
            power: self.power,
            brightness: self.brightness,
            color: self.color.clone(),
            color_temp: self.color_temp,
            mode: self.mode.clone(),
            temperature: self.temperature,
            fan_speed: self.fan_speed.clone(),
            position: self.position,
        };
        if let Some(delta) = self.brightness_delta {
            if changes.brightness.is_none() {
                let base = current
                    .and_then(|s| s.brightness)
                    .ok_or_else(|| anyhow::anyhow!("текущая яркость неизвестна"))?;
                changes.brightness = Some((i16::from(base) + i16::from(delta)).clamp(0, 100) as u8);
            }
        }
        if let Some(delta) = self.color_temp_delta {
            if changes.color_temp.is_none() {
                let base = current
                    .and_then(|s| s.color_temp)
                    .ok_or_else(|| anyhow::anyhow!("текущая температура света неизвестна"))?;
                changes.color_temp = Some((i16::from(base) + i16::from(delta)).clamp(0, 100) as u8);
            }
        }
        if let Some(delta) = self.temperature_delta {
            if changes.temperature.is_none() {
                let base = current
                    .and_then(|s| s.target_temperature.or(s.temperature))
                    .ok_or_else(|| anyhow::anyhow!("текущая температура неизвестна"))?;
                changes.temperature = Some((base + delta).clamp(5.0, 35.0));
            }
        }
        if changes.is_empty() {
            bail!("не указано, что изменить");
        }
        Ok(changes)
    }
}

fn validate_request(request: &Request) -> Result<()> {
    if request.brightness.is_some_and(|v| v > 100) {
        bail!("яркость должна быть 0..100");
    }
    if request.color_temp.is_some_and(|v| v > 100) {
        bail!("температура света должна быть 0..100");
    }
    if let Some(t) = request.temperature {
        if !(5.0..=35.0).contains(&t) {
            bail!("температура должна быть 5..35 °C");
        }
    }
    if request.position.is_some_and(|v| v > 100) {
        bail!("положение должно быть 0..100");
    }
    if let Some(color) = &request.color {
        catalog::parse_color(color).map_err(anyhow::Error::msg)?;
    }
    match request.action {
        Action::List | Action::State => {
            if request.power.is_some()
                || request.brightness.is_some()
                || request.color.is_some()
                || request.color_temp.is_some()
                || request.mode.is_some()
                || request.temperature.is_some()
                || request.fan_speed.is_some()
                || request.position.is_some()
                || request.has_deltas()
            {
                bail!(
                    "для action={:?} параметры управления не нужны",
                    request.action
                );
            }
        }
        Action::Set => {
            if request.has_deltas() {
                bail!("для action=set используйте абсолютные значения, а не delta");
            }
            if request.power.is_none()
                && request.brightness.is_none()
                && request.color.is_none()
                && request.color_temp.is_none()
                && request.mode.is_none()
                && request.temperature.is_none()
                && request.fan_speed.is_none()
                && request.position.is_none()
            {
                bail!("не указано, что изменить");
            }
        }
        Action::Adjust => {
            if !request.has_deltas() {
                bail!("для action=adjust нужен brightness_delta, color_temp_delta или temperature_delta");
            }
            if request.brightness.is_some() && request.brightness_delta.is_some()
                || request.color_temp.is_some() && request.color_temp_delta.is_some()
                || request.temperature.is_some() && request.temperature_delta.is_some()
            {
                bail!("нельзя одновременно задавать значение и delta для одного параметра");
            }
        }
    }
    Ok(())
}

fn verify(changes: &catalog::ChangeSet, state: &catalog::DeviceState) -> (bool, String) {
    let mut ok = true;
    let mut parts: Vec<String> = Vec::new();
    if let Some(power) = changes.power {
        let actual = if state.power_gangs.is_empty() {
            state.power == Some(power)
        } else {
            state.power_gangs.iter().all(|g| *g == power)
        };
        if actual {
            parts.push(
                if power {
                    "включено"
                } else {
                    "выключено"
                }
                .into(),
            );
        } else {
            ok = false;
            parts.push(
                if power {
                    "не включилось"
                } else {
                    "не выключилось"
                }
                .into(),
            );
        }
    }
    if let Some(brightness) = changes.brightness {
        match state.brightness {
            Some(actual) if actual.abs_diff(brightness) <= 2 => {
                parts.push(format!("яркость {actual}%"));
            }
            Some(actual) => {
                ok = false;
                parts.push(format!("яркость {actual}% вместо {brightness}%"));
            }
            None => {
                ok = false;
                parts.push("яркость не подтвердилась".into());
            }
        }
    }
    if let Some(color_temp) = changes.color_temp {
        match state.color_temp {
            Some(actual) if actual.abs_diff(color_temp) <= 3 => {
                parts.push("температура света изменена".into());
            }
            Some(actual) => {
                ok = false;
                parts.push(format!("температура света {actual}% вместо {color_temp}%"));
            }
            None => {
                ok = false;
                parts.push("температура света не подтвердилась".into());
            }
        }
    }
    if changes.color.is_some() {
        if state.color.is_some() {
            parts.push("цвет изменён".into());
        } else {
            ok = false;
            parts.push("цвет не подтвердился".into());
        }
    }
    if let Some(mode) = &changes.mode {
        if state
            .mode
            .as_deref()
            .is_some_and(|m| m.eq_ignore_ascii_case(mode))
        {
            parts.push("режим изменён".into());
        } else {
            ok = false;
            parts.push("режим не подтвердился".into());
        }
    }
    if let Some(temp) = changes.temperature {
        match state.target_temperature {
            Some(actual) if (actual - temp).abs() <= 0.6 => {
                parts.push(format!("температура {actual:.1} °C"));
            }
            Some(actual) => {
                ok = false;
                parts.push(format!("температура {actual:.1} °C вместо {temp:.1} °C"));
            }
            None => {
                ok = false;
                parts.push("температура не подтвердилась".into());
            }
        }
    }
    if let Some(fan) = &changes.fan_speed {
        if state
            .fan
            .as_deref()
            .is_some_and(|f| f.eq_ignore_ascii_case(fan))
        {
            parts.push("скорость вентилятора изменена".into());
        } else {
            ok = false;
            parts.push("скорость вентилятора не подтвердилась".into());
        }
    }
    if let Some(position) = changes.position {
        match state.position {
            Some(actual) if actual.abs_diff(position) <= 2 => {
                parts.push("положение изменено".into());
            }
            Some(_) => {
                ok = false;
                parts.push("положение не совпало".into());
            }
            None => {
                ok = false;
                parts.push("положение не подтвердилось".into());
            }
        }
    }
    (ok, parts.join(", "))
}

/// Печатает QR в терминале. Ошибка рендера не должна скрывать URL.
pub fn print_qr(content: &str) -> Result<()> {
    let code = qrcode::QrCode::new(content.as_bytes()).context("QR-код Tuya")?;
    let image = code
        .render::<qrcode::render::unicode::Dense1x2>()
        .quiet_zone(true)
        .build();
    println!("{image}");
    println!("Сканируйте в приложении Tuya Smart или Smart Life: + → Сканировать.");
    Ok(())
}

/// Для CLI: путь к state-каталогу интеграции.
pub fn state_dir(base: impl AsRef<Path>) -> PathBuf {
    base.as_ref().join("smart_home")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smart_home::cloud::{Session, TokenInfo, HA_CLIENT_ID};
    use crate::smart_home::model::{DpFormat, DpInfo, Room};
    use crate::smart_home::store::Store;
    use std::collections::BTreeMap;

    fn record(category: &str, room: &str) -> DeviceRecord {
        DeviceRecord {
            id: "id".into(),
            name: "Лампа".into(),
            category: category.into(),
            product_id: String::new(),
            product_name: String::new(),
            model: String::new(),
            local_key: "0123456789abcdef".into(),
            sub: false,
            gateway_id: None,
            node_id: None,
            online: true,
            room: Some(Room {
                id: "r".into(),
                name: room.into(),
            }),
            home_id: "h".into(),
            home_name: "Дом".into(),
            functions: Vec::new(),
            status: Vec::new(),
            dp_map: BTreeMap::from([(
                "1".into(),
                DpInfo {
                    dp_id: "1".into(),
                    code: "switch_led".into(),
                    format: DpFormat {
                        kind: "Bool".into(),
                        ..Default::default()
                    },
                },
            )]),
            cloud_status: BTreeMap::new(),
            local_supported: true,
            unsupported_reason: String::new(),
            ip: Some("192.168.1.2".into()),
            version: Some(ProtocolVersion::V33),
            device22: false,
            last_seen: 0,
            key_from: None,
        }
    }

    #[test]
    fn schema_has_no_device_id_or_raw_dp() {
        let json = json!({"action":"set","target":"свет","dps":{"1":true}});
        assert!(serde_json::from_value::<Request>(json).is_err());
        let json = json!({"action":"set","device_id":"id","power":true});
        assert!(serde_json::from_value::<Request>(json).is_err());
    }

    #[test]
    fn request_rejects_bad_ranges() {
        let request: Request =
            serde_json::from_value(json!({"action":"set","target":"свет","brightness":101}))
                .unwrap();
        assert!(validate_request(&request).is_err());
        let request: Request =
            serde_json::from_value(json!({"action":"adjust","target":"свет"})).unwrap();
        assert!(validate_request(&request).is_err());
    }

    #[test]
    fn deltas_need_current_state() {
        let request: Request = serde_json::from_value(json!({
            "action":"adjust","target":"свет","brightness_delta":-10
        }))
        .unwrap();
        assert!(request.to_changes(None).is_err());
        let state = catalog::DeviceState {
            brightness: Some(50),
            ..Default::default()
        };
        let changes = request.to_changes(Some(&state)).unwrap();
        assert_eq!(changes.brightness, Some(40));
    }

    #[test]
    fn visibility_hides_forbidden_and_hubs() {
        assert!(is_visible(&record("dj", "Спальня")));
        assert!(is_visible(&record("wsdcg", "Спальня")));
        assert!(!is_visible(&record("ms", "Прихожая")));
        assert!(!is_visible(&record("ipc", "Кухня")));
        assert!(is_visible(&record("pir", "Кухня")));
        assert!(!is_visible(&record("wg", "Дом")));
    }

    #[tokio::test]
    async fn set_command_is_verified_and_lock_is_blocked() {
        use crate::smart_home::lan::test_support;
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let Ok(listener) = TcpListener::bind("127.0.0.1:6668").await else {
            return;
        };
        let key = b"0123456789abcdef".to_vec();
        let server = tokio::spawn(async move {
            for step in 0..2 {
                let (mut sock, _) = listener.accept().await.unwrap();
                let (cmd, request) = test_support::read_json_outgoing(
                    &mut sock,
                    &key,
                    crate::smart_home::model::ProtocolVersion::V33,
                )
                .await
                .unwrap();
                let reply = if step == 0 {
                    assert_eq!(cmd, 7);
                    assert_eq!(request["dps"]["1"], json!(true));
                    assert!(request["dps"]["2"].as_i64().unwrap() > 400);
                    test_support::reply_55(1, 7, &[])
                } else {
                    assert_eq!(cmd, 10);
                    let json = br#"{"devId":"dev","dps":{"1":true,"2":505}}"#;
                    test_support::reply_55(1, 10, &test_support::encrypted_33(&key, json))
                };
                sock.write_all(&reply).await.unwrap();
                sock.shutdown().await.unwrap();
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("smart_home"));
        store
            .save_session(&Session {
                client_id: HA_CLIENT_ID.into(),
                user_code: "u".into(),
                terminal_id: "t".into(),
                endpoint: "https://apigw.iotbing.com".into(),
                token_info: TokenInfo {
                    t: 1,
                    uid: "u".into(),
                    expire_time: 3600,
                    access_token: "ACCESS".into(),
                    refresh_token: "REFRESH".into(),
                },
            })
            .unwrap();
        let mut lamp = record("dj", "Спальня");
        lamp.id = "lamp".into();
        lamp.name = "Лампа".into();
        lamp.ip = Some("127.0.0.1".into());
        lamp.version = Some(crate::smart_home::model::ProtocolVersion::V33);
        lamp.dp_map.insert(
            "2".into(),
            DpInfo {
                dp_id: "2".into(),
                code: "bright_value".into(),
                format: DpFormat {
                    kind: "Integer".into(),
                    scale: Some(0),
                    min: Some(10.0),
                    max: Some(1000.0),
                    step: Some(1.0),
                    unit: String::new(),
                    values: Vec::new(),
                },
            },
        );
        let mut lock = record("ms", "Прихожая");
        lock.id = "lock".into();
        lock.name = "Замок".into();
        store.save_devices(&[lamp, lock]).unwrap();
        let home = SmartHome::open(SmartHomeConfig::default(), dir.path(), "")
            .await
            .unwrap();
        let text = home
            .execute(&json!({"action":"set","target":"лампа","brightness":50}))
            .await
            .unwrap();
        assert!(text.contains("яркость 50%"), "{text}");
        assert!(home
            .execute(&json!({"action":"set","target":"замок","power":true}))
            .await
            .is_err());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn list_hides_forbidden_and_schema_has_no_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("smart_home"));
        store
            .save_session(&Session {
                client_id: HA_CLIENT_ID.into(),
                user_code: "u".into(),
                terminal_id: "t".into(),
                endpoint: "https://apigw.iotbing.com".into(),
                token_info: TokenInfo {
                    t: 1,
                    uid: "u".into(),
                    expire_time: 3600,
                    access_token: "ACCESS".into(),
                    refresh_token: "REFRESH".into(),
                },
            })
            .unwrap();
        let lamp = record("dj", "Спальня");
        let mut lock = record("ms", "Прихожая");
        lock.name = "Замок".into();
        let mut camera = record("ipc", "Кухня");
        camera.name = "Камера".into();
        store.save_devices(&[lamp, lock, camera]).unwrap();
        let home = SmartHome::open(SmartHomeConfig::default(), dir.path(), "")
            .await
            .unwrap();
        let list = home.execute(&json!({"action":"list"})).await.unwrap();
        assert!(list.contains("Лампа"), "{list}");
        assert!(!list.contains("Замок"), "{list}");
        assert!(!list.contains("Камера"), "{list}");
        assert!(list.contains("Скрыто политикой безопасности: 2"), "{list}");
        let definition = home.definition();
        assert!(!definition.function.description.contains("Замок"));
        let schema = definition.function.parameters.to_string();
        assert!(!schema.contains("device_id"));
        assert!(!schema.contains("dps"));
        let err = home
            .execute(&json!({"action":"set","target":"Замок","power":true}))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("не нашёл") || err.contains("нет разрешённых"),
            "{err}"
        );
        let err = home
            .execute(&json!({"action":"set","target":"свет","power":true,"device_id":"id"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("неверные аргументы"), "{err}");
    }

    #[test]
    fn verify_checks_actual_state() {
        let changes = catalog::ChangeSet {
            power: Some(true),
            brightness: Some(40),
            ..Default::default()
        };
        let mut state = catalog::DeviceState {
            power: Some(true),
            brightness: Some(41),
            ..Default::default()
        };
        let (ok, text) = verify(&changes, &state);
        assert!(ok, "{text}");
        state.brightness = Some(10);
        let (ok, text) = verify(&changes, &state);
        assert!(!ok);
        assert!(text.contains("10%"));
    }
}
