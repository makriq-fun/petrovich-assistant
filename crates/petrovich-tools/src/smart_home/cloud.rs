//! Облако Tuya через публичный client_id Home Assistant.
//!
//! Это только первичный вход и обновление ключей. Повседневное управление
//! идёт по локальной сети и не требует этих запросов.

use std::collections::BTreeMap;
use std::time::Duration;

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes128Gcm, KeyInit, Nonce};
use anyhow::{bail, Context, Result};
use base64::Engine;
use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use url::Url;

use super::model::{DeviceRecord, DpFormat, DpInfo, FunctionSpec, Room, StatusSpec};

/// Публичные учётные данные приложения «device sharing» Home Assistant.
/// Это не секрет: они опубликованы в открытом коде HA/tuya-local и лишь
/// обозначают схему входа. Доступ даёт только подтверждение QR в приложении.
pub const HA_CLIENT_ID: &str = "HA_3y9q4ak7g4ephrvke";
pub const DEFAULT_ENDPOINT: &str = "https://apigw.iotbing.com";
const QR_BASE: &str = "https://apigw.iotbing.com";
const QR_PATH: &str = "/v1.0/m/life/home-assistant/qrcode/tokens";

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct TokenInfo {
    #[serde(default)]
    pub t: i64,
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub expire_time: i64,
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
}

impl TokenInfo {
    fn expires_soon(&self) -> bool {
        let now = chrono::Utc::now().timestamp_millis();
        self.t + self.expire_time.saturating_mul(1000) - 60_000 <= now
    }
}

/// Сессия облака. Содержит токены, поэтому `Debug` не выводит поля.
#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    #[serde(default = "default_client_id")]
    pub client_id: String,
    #[serde(default)]
    pub user_code: String,
    #[serde(default)]
    pub terminal_id: String,
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
    #[serde(default)]
    pub token_info: TokenInfo,
}

fn default_client_id() -> String {
    HA_CLIENT_ID.into()
}
fn default_endpoint() -> String {
    DEFAULT_ENDPOINT.into()
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("user_code", &self.user_code)
            .field("terminal_id", &self.terminal_id)
            .field("endpoint", &self.endpoint)
            .field("expire_time", &self.token_info.expire_time)
            .finish_non_exhaustive()
    }
}

/// Результат QR-входа: URL для QR и сессия после подтверждения.
pub struct LoginResult {
    pub session: Session,
    pub scanned: bool,
}

pub struct CloudClient {
    http: reqwest::Client,
    pub session: Session,
    endpoint: String,
}

impl CloudClient {
    pub fn new(session: Session, timeout_ms: u64, proxy: &str) -> Result<Self> {
        let endpoint = normalize_endpoint(&session.endpoint)?;
        let http = build_http(timeout_ms, proxy)?;
        Ok(Self {
            http,
            session,
            endpoint,
        })
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn token_dirty(&self) -> bool {
        self.session.token_info.expires_soon()
    }

    /// Создаёт QR-токен и ждёт подтверждения. `qr` получает URL, который
    /// нужно показать пользователю; токен в журнал не пишется.
    pub async fn login(
        user_code: &str,
        scheme: &str,
        timeout_s: u64,
        timeout_ms: u64,
        proxy: &str,
        mut qr: impl FnMut(&str),
    ) -> Result<LoginResult> {
        Self::login_at(
            QR_BASE, user_code, scheme, timeout_s, timeout_ms, proxy, &mut qr,
        )
        .await
    }

    async fn login_at(
        base: &str,
        user_code: &str,
        scheme: &str,
        timeout_s: u64,
        timeout_ms: u64,
        proxy: &str,
        qr: &mut impl FnMut(&str),
    ) -> Result<LoginResult> {
        if user_code.trim().is_empty() {
            bail!("не указан user code приложения Tuya");
        }
        if !matches!(scheme, "smartlife" | "tuyaSmart") {
            bail!("схема входа должна быть smartlife или tuyaSmart");
        }
        let http = build_http(timeout_ms, proxy)?;
        let base = base.trim_end_matches('/');
        let url = format!(
            "{base}{QR_PATH}?clientid={HA_CLIENT_ID}&usercode={}&schema={scheme}",
            urlencode(user_code.trim())
        );
        let mut post_error = None;
        let mut response = None;
        for attempt in 0..3 {
            match http.post(&url).send().await {
                Ok(resp) => {
                    response = Some(resp);
                    break;
                }
                Err(e) => {
                    post_error = Some(network_error(e));
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        }
        let resp = response.ok_or_else(|| {
            post_error.unwrap_or_else(|| anyhow::anyhow!("облако Tuya: запрос QR не выполнен"))
        })?;
        let value = json_response(resp).await?;
        if !value
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            bail!("Tuya не выдала QR-код: {}", api_error(&value));
        }
        let token = value
            .get("result")
            .and_then(|r| r.get("qrcode"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("в ответе Tuya нет QR-токена"))?
            .to_owned();
        let content = format!("{scheme}--qrLogin?token={token}");
        qr(&content);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_s);
        let mut last_error = None;
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let poll = format!(
                "{base}{QR_PATH}/{}?clientid={HA_CLIENT_ID}&usercode={}",
                token,
                urlencode(user_code.trim())
            );
            let value = match http.get(poll).send().await {
                Ok(resp) => match json_response(resp).await {
                    Ok(value) => value,
                    Err(e) => {
                        last_error = Some(e);
                        continue;
                    }
                },
                Err(e) => {
                    // Разрыв или тайм-аут одного опроса не должен обрывать
                    // ожидание: пользователь может подтвердить QR позже.
                    last_error = Some(network_error(e));
                    continue;
                }
            };
            if value
                .get("success")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                let result = value
                    .get("result")
                    .and_then(Value::as_object)
                    .ok_or_else(|| anyhow::anyhow!("пустой ответ входа Tuya"))?;
                let session = session_from_login(result, user_code, value.get("t"))?;
                return Ok(LoginResult {
                    session,
                    scanned: true,
                });
            }
        }
        if let Some(e) = last_error {
            bail!("QR-код не подтверждён за {timeout_s} с; последняя ошибка: {e}");
        }
        bail!("QR-код не подтверждён за {timeout_s} с; запустите вход заново")
    }

    /// Обновляет access token, если он истекает. Возвращает true, если
    /// сессию нужно сохранить.
    pub async fn refresh_if_needed(&mut self) -> Result<bool> {
        if !self.session.token_info.expires_soon() {
            return Ok(false);
        }
        let path = format!("/v1.0/m/token/{}", self.session.token_info.refresh_token);
        let value = Box::pin(self.request_raw("GET", &path, None, None, true)).await?;
        if !value
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            bail!("обновление сессии Tuya отклонено: {}", api_error(&value));
        }
        let result = value
            .get("result")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow::anyhow!("пустой ответ обновления сессии"))?;
        let t = value
            .get("t")
            .and_then(Value::as_i64)
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        self.session.token_info = TokenInfo {
            t,
            uid: str_field(result, &["uid"]).unwrap_or_default(),
            expire_time: int_field(result, &["expireTime", "expire_time"]).unwrap_or(0),
            access_token: str_field(result, &["accessToken", "access_token"])
                .ok_or_else(|| anyhow::anyhow!("в обновлении нет access token"))?,
            refresh_token: str_field(result, &["refreshToken", "refresh_token"])
                .ok_or_else(|| anyhow::anyhow!("в обновлении нет refresh token"))?,
        };
        Ok(true)
    }

    pub async fn get(&mut self, path: &str, params: Option<Value>) -> Result<Value> {
        self.request_raw("GET", path, params, None, false).await
    }

    pub async fn post(&mut self, path: &str, body: Option<Value>) -> Result<Value> {
        self.request_raw("POST", path, None, body, false).await
    }

    async fn request_raw(
        &mut self,
        method: &str,
        path: &str,
        params: Option<Value>,
        body: Option<Value>,
        skip_refresh: bool,
    ) -> Result<Value> {
        if !skip_refresh && self.refresh_if_needed().await? {
            // Сессию сохранит вызывающий: login/refresh знают про Store.
        }
        let rid = uuid_v4();
        let hash_key =
            md5_hex(format!("{rid}{}", self.session.token_info.refresh_token).as_bytes());
        let secret = secret_for(&rid, &hash_key);

        let query_enc = params.map(|v| encrypt_gcm(&secret, &compact_json(&v)));
        let body_enc = body.map(|v| encrypt_gcm(&secret, &compact_json(&v)));
        let t = chrono::Utc::now().timestamp_millis();
        let mut headers: Vec<(&str, String)> = vec![
            ("X-appKey", self.session.client_id.clone()),
            ("X-requestId", rid.clone()),
            ("X-sid", String::new()),
            ("X-time", t.to_string()),
        ];
        if !self.session.token_info.access_token.is_empty() {
            headers.push(("X-token", self.session.token_info.access_token.clone()));
        }
        let sign = restful_sign(
            &hash_key,
            query_enc.as_deref().unwrap_or_default(),
            body_enc.as_deref().unwrap_or_default(),
            &headers,
        );
        let mut request = match method {
            "GET" => self.http.get(format!("{}{}", self.endpoint, path)),
            "POST" => self.http.post(format!("{}{}", self.endpoint, path)),
            _ => bail!("неподдерживаемый метод"),
        };
        for (name, value) in &headers {
            if *name == "X-sid" || !value.is_empty() {
                request = request.header(*name, value);
            }
        }
        request = request.header("X-sign", sign);
        if let Some(enc) = &query_enc {
            request = request.query(&[("encdata", enc)]);
        }
        if let Some(enc) = &body_enc {
            request = request.json(&json!({"encdata": enc}));
        }
        let resp = request.send().await.map_err(network_error)?;
        let value = json_response(resp).await?;
        if !value
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            bail!("облако Tuya: {}", api_error(&value));
        }
        let mut value = value;
        if let Some(result) = value.get_mut("result") {
            if let Some(enc) = result.as_str() {
                let plain = decrypt_gcm(&secret, enc)?;
                let parsed = serde_json::from_str(&plain).unwrap_or(Value::String(plain));
                *result = parsed;
            }
        }
        Ok(value)
    }

    /// Забирает домашние устройства, комнаты, спецификации и карту DP.
    pub async fn fetch_devices(&mut self) -> Result<Vec<DeviceRecord>> {
        let homes = self
            .get("/v1.0/m/life/users/homes", None)
            .await?
            .get("result")
            .cloned()
            .unwrap_or(Value::Null);
        let mut records: BTreeMap<String, DeviceRecord> = BTreeMap::new();
        let mut homes_seen = 0usize;
        for home in homes.as_array().cloned().unwrap_or_default() {
            let home_id = home
                .get("ownerId")
                .or_else(|| home.get("id"))
                .and_then(value_text)
                .unwrap_or_default();
            let home_name = home
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if home_id.is_empty() {
                continue;
            }
            homes_seen += 1;
            let result = self
                .get(
                    "/v1.0/m/life/ha/home/devices",
                    Some(json!({"homeId": home_id})),
                )
                .await?;
            for item in result
                .get("result")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
            {
                let Some(mut record) = parse_device(&item) else {
                    continue;
                };
                if record.home_id.is_empty() {
                    record.home_id = home_id.clone();
                }
                if record.home_name.is_empty() {
                    record.home_name = home_name.clone();
                }
                if let Ok(room) = self
                    .get(
                        &format!("/v1.0/m/thing/ha/{}/room", urlencode(&record.id)),
                        None,
                    )
                    .await
                {
                    if let Some(room) = room.get("result").and_then(parse_room) {
                        record.room = Some(room);
                    }
                }
                self.enrich(&mut record).await;
                if let Some(previous) = records.get_mut(&record.id) {
                    if previous.room.is_none() {
                        previous.room = record.room.clone();
                    }
                    continue;
                }
                records.insert(record.id.clone(), record);
            }
        }
        if homes_seen == 0 {
            bail!("в аккаунте Tuya нет домов");
        }
        let mut out: Vec<DeviceRecord> = records.into_values().collect();
        // Ключ шлюза иногда приходит только на дочернем устройстве. Если
        // облако прямо указало шлюз, берём его ключ из записи шлюза.
        let gateways: BTreeMap<String, String> = out
            .iter()
            .filter(|r| !r.local_key.is_empty())
            .map(|r| (r.id.clone(), r.local_key.clone()))
            .collect();
        for record in &mut out {
            if record.local_key.is_empty() && record.sub {
                if let Some(key) = record.gateway_id.as_ref().and_then(|id| gateways.get(id)) {
                    record.local_key = key.clone();
                    record.key_from = record.gateway_id.clone();
                }
            }
            if record.sub && record.node_id.is_none() {
                record.node_id = Some(record.id.clone());
            }
        }
        if out.is_empty() {
            bail!("в аккаунте Tuya нет устройств");
        }
        Ok(out)
    }

    async fn enrich(&mut self, record: &mut DeviceRecord) {
        // Спецификации функций.
        if let Ok(value) = self
            .get(
                &format!("/v1.1/m/life/{}/specifications", urlencode(&record.id)),
                None,
            )
            .await
        {
            if let Some(result) = value.get("result") {
                if let Some(functions) = result.get("functions").and_then(Value::as_array) {
                    record.functions = functions.iter().filter_map(parse_function).collect();
                }
                if let Some(status) = result.get("status").and_then(Value::as_array) {
                    record.status = status.iter().filter_map(parse_status).collect();
                }
            }
        } else {
            record.unsupported_reason = "облако не отдало спецификации устройства".into();
        }

        // Карта DP и разрешение локального управления.
        match self
            .get(
                &format!("/v1.0/m/life/devices/{}/status", urlencode(&record.id)),
                None,
            )
            .await
        {
            Ok(value) => {
                if let Some(result) = value.get("result") {
                    let mut support = true;
                    let mut map = BTreeMap::new();
                    if let Some(relations) =
                        result.get("dpStatusRelationDTOS").and_then(Value::as_array)
                    {
                        for rel in relations {
                            if !rel.get("supportLocal").and_then(as_boolish).unwrap_or(true) {
                                support = false;
                                break;
                            }
                            let Some(dp_id) = rel.get("dpId").and_then(value_text) else {
                                continue;
                            };
                            let Some(code) = rel.get("statusCode").and_then(Value::as_str) else {
                                continue;
                            };
                            let format = parse_status_format(
                                rel.get("statusFormat"),
                                rel.get("valueDesc"),
                                rel.get("enumMappingMap"),
                                rel.get("valueType"),
                            );
                            map.insert(
                                dp_id.clone(),
                                DpInfo {
                                    dp_id,
                                    code: code.to_owned(),
                                    format,
                                },
                            );
                        }
                    }
                    if let Ok(custom) = self
                        .get(
                            &format!("/v1.0/m/life/ha/{}/code/custom-type", urlencode(&record.id)),
                            None,
                        )
                        .await
                    {
                        if custom.get("result").and_then(as_boolish).unwrap_or(false) {
                            support = false;
                        }
                    }
                    record.local_supported =
                        support && !map.is_empty() && !record.local_key.is_empty();
                    if !support {
                        record.unsupported_reason =
                            "устройство сообщает статус через облако, не локально".into();
                    } else if map.is_empty() {
                        record.unsupported_reason =
                            "облако не сообщило карту DP для локального управления".into();
                    } else if record.local_key.is_empty() {
                        record.unsupported_reason = "в облаке нет локального ключа".into();
                    }
                    record.dp_map = map;
                    merge_spec_formats(record);
                    if record.local_supported {
                        record.unsupported_reason.clear();
                    }
                }
            }
            Err(e) => {
                record.local_supported = false;
                record.unsupported_reason = format!("не удалось получить карту DP: {e}");
            }
        }
    }
}

fn parse_device(item: &Value) -> Option<DeviceRecord> {
    let id = item.get("id").and_then(value_text)?;
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("Устройство")
        .to_owned();
    let local_key = item
        .get("local_key")
        .or_else(|| item.get("localKey"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let category = item
        .get("category")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let mut cloud_status = BTreeMap::new();
    if let Some(status) = item.get("status").and_then(Value::as_array) {
        for entry in status {
            if let (Some(code), Some(value)) = (
                entry.get("code").and_then(Value::as_str),
                entry.get("value"),
            ) {
                cloud_status.insert(code.to_owned(), value.clone());
            }
        }
    }
    let gateway_id = ["parent_id", "parentId", "gateway_id", "gatewayId"]
        .iter()
        .find_map(|k| item.get(*k).and_then(value_text));
    let node_id = ["node_id", "nodeId", "cid"]
        .iter()
        .find_map(|k| item.get(*k).and_then(value_text));
    let ip = item
        .get("ip")
        .and_then(Value::as_str)
        .filter(|ip| ip.parse::<std::net::Ipv4Addr>().is_ok_and(is_private))
        .map(str::to_owned);
    Some(DeviceRecord {
        id,
        name,
        category,
        product_id: item
            .get("product_id")
            .or_else(|| item.get("productId"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        product_name: item
            .get("product_name")
            .or_else(|| item.get("productName"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        model: item
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        local_key,
        sub: item.get("sub").and_then(as_boolish).unwrap_or(false),
        gateway_id,
        node_id,
        online: item
            .get("online")
            .and_then(Value::as_bool)
            .or_else(|| item.get("online").and_then(Value::as_i64).map(|v| v != 0))
            .unwrap_or(false),
        room: None,
        home_id: String::new(),
        home_name: String::new(),
        functions: Vec::new(),
        status: Vec::new(),
        dp_map: BTreeMap::new(),
        cloud_status,
        local_supported: false,
        unsupported_reason: String::new(),
        ip,
        version: None,
        device22: false,
        last_seen: 0,
        key_from: None,
    })
}

fn parse_room(value: &Value) -> Option<Room> {
    let id = value.get("id").and_then(value_text)?;
    let name = value.get("name").and_then(Value::as_str)?.to_owned();
    Some(Room { id, name })
}

fn parse_function(value: &Value) -> Option<FunctionSpec> {
    Some(FunctionSpec {
        code: value.get("code").and_then(Value::as_str)?.to_owned(),
        kind: value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        values: value.get("values").cloned().unwrap_or(Value::Null),
    })
}

fn parse_status(value: &Value) -> Option<StatusSpec> {
    Some(StatusSpec {
        code: value.get("code").and_then(Value::as_str)?.to_owned(),
        kind: value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        values: value.get("values").cloned().unwrap_or(Value::Null),
    })
}

fn parse_status_format(
    format: Option<&Value>,
    value_desc: Option<&Value>,
    enum_map: Option<&Value>,
    value_type: Option<&Value>,
) -> DpFormat {
    let mut out = DpFormat::default();
    if let Some(text) = format.and_then(Value::as_str) {
        if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(text) {
            fill_format(&mut out, &Value::Object(obj));
        }
    } else if let Some(obj) = format.filter(|v| v.is_object()) {
        fill_format(&mut out, obj);
    }
    if out.kind.is_empty() {
        out.kind = value_type
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
    }
    if out.values.is_empty() {
        if let Some(desc) = value_desc.and_then(Value::as_str) {
            if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(desc) {
                out.values = arr
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect();
            }
        }
    }
    if out.values.is_empty() {
        if let Some(map) = enum_map.and_then(Value::as_object) {
            out.values = map
                .values()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
        }
    }
    out
}

/// Карта DP приходит от Tuya без min/max/range, а спецификации функций —
/// с ними. Дополняем формат, не перезаписывая уже известные поля.
fn merge_spec_formats(record: &mut DeviceRecord) {
    let mut specs = BTreeMap::new();
    for spec in &record.functions {
        specs.insert(spec.code.clone(), spec.values.clone());
    }
    for spec in &record.status {
        specs.insert(spec.code.clone(), spec.values.clone());
    }
    for dp in record.dp_map.values_mut() {
        let Some(values) = specs.get(&dp.code) else {
            continue;
        };
        let parsed = match values {
            Value::String(text) => serde_json::from_str::<Value>(text).ok(),
            Value::Object(_) => Some(values.clone()),
            _ => None,
        };
        if let Some(Value::Object(obj)) = parsed {
            fill_format(&mut dp.format, &Value::Object(obj));
        }
    }
}

fn fill_format(out: &mut DpFormat, obj: &Value) {
    if out.kind.is_empty() {
        out.kind = obj
            .get("type")
            .or_else(|| obj.get("valueType"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
    }
    if out.min.is_none() {
        out.min = obj.get("min").and_then(Value::as_f64);
    }
    if out.max.is_none() {
        out.max = obj.get("max").and_then(Value::as_f64);
    }
    if out.scale.is_none() {
        out.scale = obj
            .get("scale")
            .and_then(Value::as_i64)
            .and_then(|v| i32::try_from(v).ok());
    }
    if out.step.is_none() {
        out.step = obj.get("step").and_then(Value::as_f64);
    }
    if out.unit.is_empty() {
        out.unit = obj
            .get("unit")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
    }
    if out.values.is_empty() {
        if let Some(range) = obj.get("range").and_then(Value::as_array) {
            out.values = range
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
        }
    }
}

fn session_from_login(
    result: &serde_json::Map<String, Value>,
    user_code: &str,
    t: Option<&Value>,
) -> Result<Session> {
    let endpoint = result
        .get("endpoint")
        .or_else(|| result.get("end_point"))
        .or_else(|| result.get("endPoint"))
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_ENDPOINT)
        .to_owned();
    let token_info = TokenInfo {
        t: t.and_then(Value::as_i64)
            .or_else(|| result.get("t").and_then(Value::as_i64))
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
        uid: str_field(result, &["uid"]).unwrap_or_default(),
        expire_time: int_field(result, &["expire_time", "expireTime"]).unwrap_or(0),
        access_token: str_field(result, &["access_token", "accessToken"])
            .ok_or_else(|| anyhow::anyhow!("в ответе входа нет access token"))?,
        refresh_token: str_field(result, &["refresh_token", "refreshToken"])
            .ok_or_else(|| anyhow::anyhow!("в ответе входа нет refresh token"))?,
    };
    if token_info.access_token.is_empty() || token_info.refresh_token.is_empty() {
        bail!("облако Tuya вернуло пустые токены");
    }
    Ok(Session {
        client_id: HA_CLIENT_ID.into(),
        user_code: user_code.trim().to_owned(),
        terminal_id: str_field(result, &["terminal_id", "terminalId"]).unwrap_or_default(),
        endpoint,
        token_info,
    })
}

fn str_field(map: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| map.get(*k).and_then(value_text).filter(|v| !v.is_empty()))
}

fn int_field(map: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|k| {
        map.get(*k).and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
    })
}

fn as_boolish(value: &Value) -> Option<bool> {
    value
        .as_bool()
        .or_else(|| value.as_i64().map(|v| v != 0))
        .or_else(|| {
            value
                .as_str()
                .map(|s| matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
        })
}

fn value_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn is_private(ip: std::net::Ipv4Addr) -> bool {
    ip.is_private() || ip.is_link_local() || ip.is_loopback()
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{}".into())
}

/// Секрет запроса: md5(requestId + refreshToken), затем HMAC-SHA256 с ключом
/// requestId; первые 16 hex-символов — ключ AES-128-GCM.
pub fn secret_for(rid: &str, hash_key: &str) -> String {
    let bytes = hmac_sha256(rid.as_bytes(), hash_key.as_bytes());
    hex(&bytes)[..16].to_owned()
}

/// Подпись запроса в формате Tuya.
pub fn restful_sign(
    hash_key: &str,
    query_encdata: &str,
    body_encdata: &str,
    headers: &[(&str, String)],
) -> String {
    let mut header_sign = String::new();
    for name in ["X-appKey", "X-requestId", "X-sid", "X-time", "X-token"] {
        let value = headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
            .unwrap_or_default();
        if !value.is_empty() {
            header_sign.push_str(name);
            header_sign.push('=');
            header_sign.push_str(value);
            header_sign.push_str("||");
        }
    }
    let mut sign = header_sign
        .strip_suffix("||")
        .unwrap_or(&header_sign)
        .to_owned();
    sign.push_str(query_encdata);
    sign.push_str(body_encdata);
    hex(&hmac_sha256(hash_key.as_bytes(), sign.as_bytes()))
}

fn encrypt_gcm(secret: &str, plain: &str) -> String {
    let mut nonce = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce);
    let cipher = Aes128Gcm::new_from_slice(secret.as_bytes()).expect("16-byte key");
    let encrypted = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plain.as_bytes(),
                aad: &[],
            },
        )
        .expect("GCM encrypt");
    format!(
        "{}{}",
        base64::engine::general_purpose::STANDARD.encode(nonce),
        base64::engine::general_purpose::STANDARD.encode(encrypted)
    )
}

fn decrypt_gcm(secret: &str, encoded: &str) -> Result<String> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("base64 ответа Tuya")?;
    if raw.len() < 12 + 16 {
        bail!("короткий ответ Tuya");
    }
    let cipher = Aes128Gcm::new_from_slice(secret.as_bytes()).context("ключ ответа Tuya")?;
    let plain = cipher
        .decrypt(
            Nonce::from_slice(&raw[..12]),
            Payload {
                msg: &raw[12..],
                aad: &[],
            },
        )
        .map_err(|_| anyhow::anyhow!("не удалось расшифровать ответ Tuya"))?;
    String::from_utf8(plain).context("ответ Tuya не UTF-8")
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC key");
    mac.update(data);
    let out = mac.finalize().into_bytes();
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&out);
    buf
}

fn md5_hex(data: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.update(data);
    hex(&hasher.finalize())
}

fn hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{}-{}-{}-{}-{}",
        hex(&bytes[0..4]),
        hex(&bytes[4..6]),
        hex(&bytes[6..8]),
        hex(&bytes[8..10]),
        hex(&bytes[10..16])
    )
}

fn normalize_endpoint(endpoint: &str) -> Result<String> {
    let url = Url::parse(endpoint).context("endpoint Tuya")?;
    if url.scheme() != "https" {
        bail!("endpoint Tuya должен быть HTTPS");
    }
    let host = url.host_str().unwrap_or_default();
    if !(host == "apigw.iotbing.com"
        || host.ends_with(".iotbing.com")
        || host.ends_with(".tuyaeu.com")
        || host.ends_with(".tuyacn.com")
        || host.ends_with(".tuya.com"))
    {
        bail!("недопустимый endpoint Tuya");
    }
    Ok(endpoint.trim_end_matches('/').to_owned())
}

fn build_http(timeout_ms: u64, proxy: &str) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("petrovich-assistant/", env!("CARGO_PKG_VERSION")));
    let proxy = proxy.trim();
    if !proxy.is_empty() && proxy != "direct" {
        builder =
            builder.proxy(reqwest::Proxy::all(proxy).with_context(|| format!("прокси {proxy}"))?);
    }
    builder.build().context("HTTP-клиент Tuya")
}

async fn json_response(resp: reqwest::Response) -> Result<Value> {
    if !resp.status().is_success() {
        bail!("облако Tuya: HTTP {}", resp.status().as_u16());
    }
    resp.json::<Value>().await.context("разбор ответа Tuya")
}

fn api_error(value: &Value) -> String {
    let code = value.get("code").and_then(value_text).unwrap_or_default();
    let msg = value
        .get("msg")
        .and_then(Value::as_str)
        .unwrap_or("неизвестная ошибка");
    if code.is_empty() {
        msg.to_owned()
    } else {
        format!("{msg} (код {code})")
    }
}

fn network_error(error: reqwest::Error) -> anyhow::Error {
    // Display у reqwest может содержать URL с QR-токеном, поэтому источник
    // не прикладываем: только класс ошибки.
    if error.is_timeout() {
        anyhow::anyhow!("облако Tuya: тайм-аут")
    } else if error.is_connect() {
        anyhow::anyhow!("облако Tuya: нет соединения")
    } else {
        anyhow::anyhow!("облако Tuya: сетевая ошибка")
    }
}

fn urlencode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smart_home::testutil::http_response;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn signing_matches_tuya_sdk_contract() {
        let hash_key = "0123456789abcdef0123456789abcdef";
        let headers = vec![
            ("X-appKey", HA_CLIENT_ID.to_string()),
            (
                "X-requestId",
                "11111111-2222-4333-8444-555555555555".to_string(),
            ),
            ("X-sid", String::new()),
            ("X-time", "1700000000000".to_string()),
            ("X-token", "access".to_string()),
        ];
        // Подпись проверяется независимо: HMAC-SHA256 от канонической строки.
        let sign = restful_sign(hash_key, "query-enc", "body-enc", &headers);
        let expected = {
            let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(hash_key.as_bytes()).unwrap();
            mac.update(
                format!(
                    "X-appKey={}||X-requestId=11111111-2222-4333-8444-555555555555||X-time=1700000000000||X-token=accessquery-encbody-enc",
                    HA_CLIENT_ID
                )
                .as_bytes(),
            );
            hex(&mac.finalize().into_bytes())
        };
        assert_eq!(sign, expected);
        assert_eq!(
            secret_for("rid", "hash"),
            hex(&hmac_sha256(b"rid", b"hash"))[..16]
        );
    }

    #[test]
    fn gcm_roundtrip_and_tamper_rejected() {
        let secret = "0123456789abcdef";
        let encoded = encrypt_gcm(secret, "{\"ok\":true}");
        assert_eq!(decrypt_gcm(secret, &encoded).unwrap(), "{\"ok\":true}");
        let mut broken = encoded.into_bytes();
        let last = broken.len() - 2;
        broken[last] = if broken[last] == b'A' { b'B' } else { b'A' };
        assert!(decrypt_gcm(secret, &String::from_utf8(broken).unwrap()).is_err());
    }

    #[test]
    fn session_from_login_accepts_camel_case() {
        let map = serde_json::json!({
            "terminal_id": "term",
            "endpoint": "https://apigw.iotbing.com",
            "access_token": "ACCESS-SECRET",
            "refresh_token": "REFRESH-SECRET",
            "expire_time": 3600,
            "uid": "u"
        });
        let map = map.as_object().unwrap();
        let session = session_from_login(map, "code", Some(&json!(100))).unwrap();
        assert_eq!(session.token_info.access_token, "ACCESS-SECRET");
        assert_eq!(session.token_info.expire_time, 3600);
        let debug = format!("{session:?}");
        assert!(!debug.contains("ACCESS-SECRET"));
        assert!(!debug.contains("REFRESH-SECRET"));
    }

    #[test]
    fn token_expiry_uses_sdk_semantics() {
        let now = chrono::Utc::now().timestamp_millis();
        let fresh = TokenInfo {
            t: now,
            expire_time: 3600,
            ..Default::default()
        };
        assert!(!fresh.expires_soon());
        let old = TokenInfo {
            t: now - 3_600_000,
            expire_time: 3600,
            ..Default::default()
        };
        assert!(old.expires_soon());
    }

    #[test]
    fn function_specs_fill_missing_dp_ranges() {
        let mut record: DeviceRecord = serde_json::from_value(json!({
            "id": "device",
            "name": "Лампа",
            "category": "dj",
            "functions": [{
                "code": "bright_value_v2",
                "kind": "Integer",
                "values": "{\"min\":10,\"max\":1000,\"scale\":0,\"step\":1}"
            }],
            "dp_map": {
                "22": {
                    "dp_id": "22",
                    "code": "bright_value_v2",
                    "format": {"kind": "Integer"}
                }
            }
        }))
        .unwrap();
        merge_spec_formats(&mut record);
        let format = &record.dp_map["22"].format;
        assert_eq!(format.min, Some(10.0));
        assert_eq!(format.max, Some(1000.0));
        assert_eq!(format.step, Some(1.0));
        assert_eq!(format.scale, Some(0));
    }

    #[test]
    fn endpoint_must_be_tuya_https() {
        assert!(normalize_endpoint("https://apigw.iotbing.com").is_ok());
        assert!(normalize_endpoint("http://apigw.iotbing.com").is_err());
        assert!(normalize_endpoint("https://evil.example").is_err());
    }

    #[test]
    fn direct_proxy_is_supported() {
        assert!(build_http(1000, "direct").is_ok());
    }

    #[tokio::test]
    async fn qr_login_polls_and_parses_session() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            for step in 0..2 {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap();
                let request = String::from_utf8_lossy(&buf[..n]);
                assert!(request.contains("/v1.0/m/life/home-assistant/qrcode/tokens"));
                let body = if step == 0 {
                    assert!(request.contains("schema=smartlife"), "{request}");
                    json!({"success": true, "result": {"qrcode": "qr-token"}})
                } else {
                    assert!(request.contains("qr-token"));
                    json!({
                        "success": true,
                        "t": 1700000000000i64,
                        "result": {
                            "terminal_id": "term",
                            "endpoint": "https://apigw.iotbing.com",
                            "access_token": "access",
                            "refresh_token": "refresh",
                            "expire_time": 3600,
                            "uid": "u"
                        }
                    })
                };
                let response = http_response(&body.to_string());
                sock.write_all(response.as_bytes()).await.unwrap();
                sock.shutdown().await.unwrap();
            }
        });
        let mut shown = String::new();
        let mut http = build_http(5000, "").unwrap();
        let _ = &mut http;
        // Ускоряем ожидание: login_at спит 2 с между опросами.
        let session = tokio::time::timeout(
            Duration::from_secs(10),
            CloudClient::login_at(&base, "user-code", "smartlife", 10, 5000, "", &mut |url| {
                shown = url.to_owned();
            }),
        )
        .await
        .unwrap()
        .unwrap();
        handle.await.unwrap();
        assert!(shown.starts_with("smartlife--qrLogin?token="));
        assert_eq!(session.session.token_info.access_token, "access");
        assert_eq!(session.session.terminal_id, "term");
    }

    #[test]
    fn crypto_matches_reference_vector() {
        let rid = "123e4567-e89b-12d3-a456-426614174000";
        let hash_key = md5_hex(format!("{rid}REFRESH").as_bytes());
        assert_eq!(hash_key, "b676eaaa75359b35152627bb11477c6e");
        assert_eq!(secret_for(rid, &hash_key), "0b2f60938b632a50");
        let headers = vec![
            ("X-appKey", "HA_3y9q4ak7g4ephrvke".to_owned()),
            ("X-requestId", rid.to_owned()),
            ("X-sid", String::new()),
            ("X-time", "1700000000000".to_owned()),
            ("X-token", "ACCESS".to_owned()),
        ];
        assert_eq!(
            restful_sign(&hash_key, "QUERYENC", "", &headers),
            "76e5e14f043c5d630dacba5aa338b267a1d280294338593d95b1839ea1745222"
        );
    }

    #[tokio::test]
    async fn signed_get_decrypts_result() {
        // Проверяем, что клиент отправляет X-sign и расшифровывает encdata.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16384];
            let n = sock.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
            assert!(request.contains("x-sign:"));
            let rid = request
                .lines()
                .find_map(|line| line.strip_prefix("x-requestid:"))
                .map(str::trim)
                .unwrap()
                .to_owned();
            let hash_key = md5_hex(format!("{rid}r").as_bytes());
            let secret = secret_for(&rid, &hash_key);
            let encrypted = encrypt_gcm(&secret, "{\"a\":1}");
            let response =
                http_response(&format!("{{\"success\":true,\"result\":\"{encrypted}\"}}"));
            sock.write_all(response.as_bytes()).await.unwrap();
            sock.shutdown().await.unwrap();
        });
        let session = Session {
            client_id: HA_CLIENT_ID.into(),
            user_code: "u".into(),
            terminal_id: "t".into(),
            endpoint: base.clone(),
            token_info: TokenInfo {
                t: chrono::Utc::now().timestamp_millis(),
                expire_time: 3600,
                access_token: "a".into(),
                refresh_token: "r".into(),
                uid: "u".into(),
            },
        };
        // Для теста разрешаем HTTP-endpoint локального сервера.
        let mut client = CloudClient {
            http: build_http(5000, "").unwrap(),
            session,
            endpoint: base,
        };
        let value = client
            .get("/v1.0/m/test", Some(json!({"a": 1})))
            .await
            .unwrap();
        assert!(value.get("success").unwrap().as_bool().unwrap());
        assert_eq!(value["result"]["a"], json!(1));
        handle.await.unwrap();
    }
}
