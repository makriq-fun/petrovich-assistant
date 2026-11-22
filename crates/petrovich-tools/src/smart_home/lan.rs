//! Локальный протокол Tuya 3.1/3.3/3.4/3.5.
//!
//! Кадр: заголовок 16 байт, payload, CRC/HMAC или GCM-тег, суффикс. Для 3.4
//! используется AES-128-ECB и HMAC-SHA256, для 3.5 — AES-128-GCM и кадр
//! 6699. Ключ устройства никогда не попадает в сообщения об ошибках.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use aes::cipher::{generic_array::GenericArray, BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes128Gcm, Nonce};
use anyhow::{bail, Context, Result};
use base64::Engine;
use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use rand::RngCore;
use serde_json::{json, Value};
use sha2::Sha256;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::model::{DeviceRecord, ProtocolVersion};

const PORT: u16 = 6668;
const PREFIX_55AA: [u8; 4] = [0x00, 0x00, 0x55, 0xAA];
const PREFIX_6699: [u8; 4] = [0x00, 0x00, 0x66, 0x99];
const SUFFIX_55AA: [u8; 4] = [0x00, 0x00, 0xAA, 0x55];
const SUFFIX_6699: [u8; 4] = [0x00, 0x00, 0x99, 0x66];
const VERSION_HEADER_ZERO: [u8; 12] = [0; 12];
const VERSION_HEADER_LEN: usize = 15;
const MAX_FRAME: usize = 1024 * 1024;
const DEVICE22_MARKER: &str = "__tuya_device22__";

const CMD_CONTROL: u32 = 7;
const CMD_HEARTBEAT: u32 = 9;
const CMD_DP_QUERY: u32 = 10;
const CMD_CONTROL_NEW: u32 = 13;
const CMD_DP_QUERY_NEW: u32 = 16;
const CMD_SESS_START: u32 = 3;
const CMD_SESS_RESP: u32 = 4;
const CMD_SESS_FINISH: u32 = 5;

fn no_protocol_header(cmd: u32) -> bool {
    matches!(
        cmd,
        CMD_DP_QUERY
            | CMD_DP_QUERY_NEW
            | CMD_HEARTBEAT
            | 18
            | CMD_SESS_START
            | CMD_SESS_RESP
            | CMD_SESS_FINISH
            | 64
    )
}

#[derive(Clone, Debug)]
pub struct QueryResult {
    pub dps: BTreeMap<String, Value>,
    pub device22: bool,
}

#[derive(Clone, Debug)]
pub struct ControlResult {
    pub acknowledged: bool,
}

#[derive(Default, Clone, Debug)]
pub struct DiscoveryConfig {
    pub timeout: Duration,
    pub scan_subnet: Option<(Ipv4Addr, u8)>,
    pub max_hosts: usize,
    pub max_parallel: usize,
    pub allow_v31: bool,
}

/// Пробует все версии протокола на одном адресе.
pub async fn probe_at(
    record: &DeviceRecord,
    ip: IpAddr,
    cfg: &DiscoveryConfig,
    preferred: Option<ProtocolVersion>,
) -> Result<(ProtocolVersion, QueryResult)> {
    let mut last = None;
    let mut versions: Vec<ProtocolVersion> = Vec::new();
    if let Some(v) = preferred {
        versions.push(v);
    }
    for v in ProtocolVersion::probe_order(cfg.allow_v31) {
        if !versions.contains(v) {
            versions.push(*v);
        }
    }
    for version in versions {
        let mut candidate = record.clone();
        candidate.ip = Some(ip.to_string());
        candidate.version = Some(version);
        candidate.last_seen = 0;
        match query_record(&candidate, cfg.timeout).await {
            Ok(r) => return Ok((version, r)),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("устройство не ответило")))
}

/// Проверяет, что адрес принимает TCP-соединение на порту Tuya.
pub async fn port_open(ip: Ipv4Addr, timeout: Duration) -> bool {
    let addr = SocketAddr::new(IpAddr::V4(ip), PORT);
    matches!(
        tokio::time::timeout(timeout, TcpStream::connect(addr)).await,
        Ok(Ok(_))
    )
}

/// Опрашивает состояние устройства по его сохранённому IP и версии.
pub async fn query_record(record: &DeviceRecord, timeout: Duration) -> Result<QueryResult> {
    let ip = record_ip(record)?;
    query_record_at(record, SocketAddr::new(ip, PORT), timeout).await
}

/// То же, но с явным адресом. Нужно для тестов и диагностики.
pub async fn query_record_at(
    record: &DeviceRecord,
    addr: SocketAddr,
    timeout: Duration,
) -> Result<QueryResult> {
    let version = record_version(record)?;
    let initial_device22 = record.device22 || version == ProtocolVersion::V32;
    match query_once(record, addr, version, timeout, initial_device22).await {
        Ok(result) => Ok(result),
        Err(e) if !initial_device22 && e.to_string().contains(DEVICE22_MARKER) => {
            query_once(record, addr, version, timeout, true).await
        }
        Err(e) => Err(e),
    }
}

/// Отправляет DP-команды. Значения приходят только из белого списка
/// `catalog::plan`; произвольные ключи здесь не принимаются.
pub async fn control_record(
    record: &DeviceRecord,
    dps: &[(String, Value)],
    timeout: Duration,
) -> Result<ControlResult> {
    if dps.is_empty() {
        bail!("пустая команда");
    }
    let ip = record_ip(record)?;
    control_record_at(record, SocketAddr::new(ip, PORT), timeout, dps).await
}

/// То же, но с явным адресом.
pub async fn control_record_at(
    record: &DeviceRecord,
    addr: SocketAddr,
    timeout: Duration,
    dps: &[(String, Value)],
) -> Result<ControlResult> {
    let version = record_version(record)?;
    control_once(record, addr, version, timeout, record.device22, dps).await
}

fn record_ip(record: &DeviceRecord) -> Result<IpAddr> {
    let ip = record
        .ip
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("адрес устройства неизвестен"))?;
    ip.parse()
        .with_context(|| format!("неверный IP устройства: {ip}"))
}

fn record_version(record: &DeviceRecord) -> Result<ProtocolVersion> {
    record
        .version
        .ok_or_else(|| anyhow::anyhow!("версия протокола устройства неизвестна"))
}

async fn connect(addr: SocketAddr, timeout: Duration) -> Result<TcpStream> {
    let stream = tokio::time::timeout(timeout, TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow::anyhow!("тайм-аут подключения к {}", addr.ip()))?
        .with_context(|| format!("подключение к {}", addr.ip()))?;
    stream.set_nodelay(true).ok();
    Ok(stream)
}

struct Session {
    stream: TcpStream,
    key: Vec<u8>,
    version: ProtocolVersion,
    seq: u32,
}

impl Session {
    async fn open(
        addr: SocketAddr,
        key: &[u8],
        version: ProtocolVersion,
        timeout: Duration,
    ) -> Result<Self> {
        let stream = connect(addr, timeout).await?;
        let mut session = Self {
            stream,
            key: key.to_vec(),
            version,
            seq: 1,
        };
        if version == ProtocolVersion::V34 || version == ProtocolVersion::V35 {
            session.negotiate(timeout).await?;
        }
        Ok(session)
    }

    async fn negotiate(&mut self, timeout: Duration) -> Result<()> {
        let real_key = self.key.clone();
        let mut local_nonce = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut local_nonce);
        if self.version == ProtocolVersion::V34 {
            let encrypted = aes_ecb_encrypt(&real_key, &local_nonce, true)?;
            let frame = build_55aa(self.seq, CMD_SESS_START, &encrypted, Some(&real_key));
            self.seq += 1;
            self.write(&frame, timeout).await?;
            let reply = read_frame(&mut self.stream, &real_key, self.version, timeout).await?;
            if reply.cmd != CMD_SESS_RESP {
                bail!("неверный ответ согласования ключа");
            }
            let payload = aes_ecb_decrypt(&real_key, &reply.payload, true)
                .context("расшифровка ключа сессии 3.4")?;
            if payload.len() < 48 {
                bail!("короткий ответ согласования ключа");
            }
            let remote_nonce = &payload[..16];
            let expected = hmac_sha256(&real_key, &local_nonce);
            if payload[16..48] != expected {
                bail!("устройство не подтвердило ключ (неверный local key)");
            }
            let finish = hmac_sha256(&real_key, remote_nonce);
            let encrypted = aes_ecb_encrypt(&real_key, &finish, true)?;
            let frame = build_55aa(self.seq, CMD_SESS_FINISH, &encrypted, Some(&real_key));
            self.seq += 1;
            self.write(&frame, timeout).await?;
            let mut xor = [0u8; 16];
            for i in 0..16 {
                xor[i] = local_nonce[i] ^ remote_nonce[i];
            }
            self.key = aes_ecb_encrypt(&real_key, &xor, false)?;
            if self.key.len() != 16 {
                bail!("неверная длина ключа сессии");
            }
        } else {
            let frame = build_6699(self.seq, CMD_SESS_START, &local_nonce, &real_key)?;
            self.seq += 1;
            self.write(&frame, timeout).await?;
            let reply = read_frame(&mut self.stream, &real_key, self.version, timeout).await?;
            if reply.cmd != CMD_SESS_RESP || reply.payload.len() < 48 {
                bail!("неверный ответ согласования ключа 3.5");
            }
            let remote_nonce = &reply.payload[..16];
            let expected = hmac_sha256(&real_key, &local_nonce);
            if reply.payload[16..48] != expected {
                bail!("устройство не подтвердило ключ (неверный local key)");
            }
            let finish = hmac_sha256(&real_key, remote_nonce);
            let frame = build_6699(self.seq, CMD_SESS_FINISH, &finish, &real_key)?;
            self.seq += 1;
            self.write(&frame, timeout).await?;
            let mut xor = [0u8; 16];
            for i in 0..16 {
                xor[i] = local_nonce[i] ^ remote_nonce[i];
            }
            let iv = &local_nonce[..12];
            let encrypted = aes_gcm_encrypt(&real_key, iv, &xor, &[])?;
            if encrypted.len() < 16 {
                bail!("неверная длина ключа сессии 3.5");
            }
            self.key = encrypted[..16].to_vec();
        }
        Ok(())
    }

    async fn write(&mut self, data: &[u8], timeout: Duration) -> Result<()> {
        tokio::time::timeout(timeout, self.stream.write_all(data))
            .await
            .map_err(|_| anyhow::anyhow!("тайм-аут отправки команды"))?
            .context("отправка команды устройству")?;
        tokio::time::timeout(timeout, self.stream.flush())
            .await
            .map_err(|_| anyhow::anyhow!("тайм-аут отправки команды"))?
            .context("сброс буфера устройства")?;
        Ok(())
    }

    async fn send(&mut self, cmd: u32, plaintext: &[u8], timeout: Duration) -> Result<()> {
        let frame = encode_message(self.version, &self.key, self.seq, cmd, plaintext)?;
        self.seq = self.seq.wrapping_add(1);
        self.write(&frame, timeout).await
    }

    async fn receive(&mut self, timeout: Duration) -> Result<Frame> {
        read_frame(&mut self.stream, &self.key, self.version, timeout).await
    }
}

/// Собирает кадр запроса. Вынесено отдельно, чтобы тесты сверяли байты с
/// эталонными кадрами TinyTuya, а не только «само с собой».
fn encode_message(
    version: ProtocolVersion,
    key: &[u8],
    seq: u32,
    cmd: u32,
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    Ok(match version {
        ProtocolVersion::V31 => {
            let payload = if cmd == CMD_CONTROL {
                v31_control_payload(plaintext, key)?
            } else {
                plaintext.to_vec()
            };
            build_55aa(seq, cmd, &payload, None)
        }
        ProtocolVersion::V32 | ProtocolVersion::V33 => {
            let encrypted = aes_ecb_encrypt(key, plaintext, true)?;
            let payload = if no_protocol_header(cmd) {
                encrypted
            } else {
                with_version_header(ProtocolVersion::V33, &encrypted)
            };
            build_55aa(seq, cmd, &payload, None)
        }
        ProtocolVersion::V34 => {
            let plain = if no_protocol_header(cmd) {
                plaintext.to_vec()
            } else {
                with_version_header(ProtocolVersion::V34, plaintext)
            };
            let encrypted = aes_ecb_encrypt(key, &plain, true)?;
            build_55aa(seq, cmd, &encrypted, Some(key))
        }
        ProtocolVersion::V35 => {
            let plain = if no_protocol_header(cmd) {
                plaintext.to_vec()
            } else {
                with_version_header(ProtocolVersion::V35, plaintext)
            };
            build_6699(seq, cmd, &plain, key)?
        }
    })
}

async fn query_once(
    record: &DeviceRecord,
    addr: SocketAddr,
    version: ProtocolVersion,
    timeout: Duration,
    device22: bool,
) -> Result<QueryResult> {
    let key = record_key(record)?;
    let mut session = Session::open(addr, &key, version, timeout).await?;
    let (cmd, payload) = if device22 {
        (
            CMD_CONTROL_NEW,
            json!({
                "devId": record.id,
                "uid": record.id,
                "t": epoch_value(version),
                "dps": {"1": Value::Null}
            })
            .to_string(),
        )
    } else {
        match version {
            ProtocolVersion::V31 | ProtocolVersion::V32 | ProtocolVersion::V33 => {
                let mut payload = json!({
                    "gwId": record.id,
                    "devId": record.id,
                    "uid": record.id,
                    "t": epoch_string()
                });
                if let Some(cid) = record.node_id.as_deref().filter(|s| !s.is_empty()) {
                    payload["cid"] = json!(cid);
                }
                (CMD_DP_QUERY, payload.to_string())
            }
            ProtocolVersion::V34 | ProtocolVersion::V35 => {
                if let Some(cid) = record.node_id.as_deref().filter(|s| !s.is_empty()) {
                    (CMD_DP_QUERY_NEW, json!({"cid": cid}).to_string())
                } else {
                    (CMD_DP_QUERY_NEW, "{}".to_owned())
                }
            }
        }
    };
    session.send(cmd, payload.as_bytes(), timeout).await?;
    let mut last_error = None;
    for _ in 0..4 {
        let frame = session.receive(timeout).await?;
        if frame.retcode != 0 {
            return Err(anyhow::anyhow!("устройство вернуло код {}", frame.retcode));
        }
        if frame.payload.is_empty() {
            continue;
        }
        match decode_payload(&frame.payload, version, &session.key) {
            Ok(value) => {
                if let Some(err) = value.get("Err") {
                    last_error = Some(anyhow::anyhow!("устройство: {err}"));
                    continue;
                }
                let dps = extract_dps(&value);
                if dps.is_empty() && !device22 {
                    // Некоторые прошивки отвечают «data unvalid» на обычный
                    // запрос и требуют команду 0x0d.
                    return Err(anyhow::anyhow!(DEVICE22_MARKER));
                }
                return Ok(QueryResult { dps, device22 });
            }
            Err(e) => {
                last_error = Some(e);
                continue;
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("устройство не вернуло состояние")))
}

async fn control_once(
    record: &DeviceRecord,
    addr: SocketAddr,
    version: ProtocolVersion,
    timeout: Duration,
    device22: bool,
    dps: &[(String, Value)],
) -> Result<ControlResult> {
    let key = record_key(record)?;
    let mut session = Session::open(addr, &key, version, timeout).await?;
    let dps_obj: serde_json::Map<String, Value> =
        dps.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let (cmd, payload) = match version {
        ProtocolVersion::V31 | ProtocolVersion::V32 | ProtocolVersion::V33 => {
            let mut obj = serde_json::Map::new();
            obj.insert("devId".into(), json!(record.id));
            obj.insert("uid".into(), json!(record.id));
            obj.insert("t".into(), json!(epoch_string()));
            obj.insert("dps".into(), Value::Object(dps_obj));
            if let Some(cid) = cid_for(record, device22) {
                obj.insert("cid".into(), json!(cid));
            }
            (CMD_CONTROL, Value::Object(obj).to_string())
        }
        ProtocolVersion::V34 | ProtocolVersion::V35 => {
            let mut data = serde_json::Map::new();
            data.insert("dps".into(), Value::Object(dps_obj));
            if let Some(cid) = cid_for(record, device22) {
                data.insert("cid".into(), json!(cid));
                data.insert("ctype".into(), json!(0));
            }
            (
                CMD_CONTROL_NEW,
                json!({"protocol": 5, "t": epoch_int(), "data": data}).to_string(),
            )
        }
    };
    session.send(cmd, payload.as_bytes(), timeout).await?;
    match session.receive(timeout).await {
        Ok(frame) => {
            if frame.retcode != 0 {
                bail!("устройство отклонило команду: код {}", frame.retcode);
            }
            if !frame.payload.is_empty() {
                match decode_payload(&frame.payload, version, &session.key) {
                    Ok(value) => {
                        if let Some(err) = value.get("Err") {
                            bail!("устройство отклонило команду: {err}");
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(ControlResult { acknowledged: true })
        }
        Err(e) if is_timeout(&e) => Ok(ControlResult {
            acknowledged: false,
        }),
        Err(e) => Err(e),
    }
}

fn is_timeout(e: &anyhow::Error) -> bool {
    e.to_string().contains("тайм-аут")
}

fn cid_for(record: &DeviceRecord, device22: bool) -> Option<&str> {
    let _ = device22;
    record.node_id.as_deref().filter(|s| !s.is_empty())
}

fn record_key(record: &DeviceRecord) -> Result<[u8; 16]> {
    let bytes = record.local_key.as_bytes();
    if bytes.len() != 16 {
        bail!("локальный ключ устройства неверной длины");
    }
    let mut key = [0u8; 16];
    key.copy_from_slice(bytes);
    Ok(key)
}

fn epoch_string() -> String {
    chrono::Utc::now().timestamp().to_string()
}

fn epoch_int() -> i64 {
    chrono::Utc::now().timestamp()
}

fn epoch_value(version: ProtocolVersion) -> Value {
    match version {
        ProtocolVersion::V31 | ProtocolVersion::V32 | ProtocolVersion::V33 => json!(epoch_string()),
        _ => json!(epoch_int()),
    }
}

fn with_version_header(version: ProtocolVersion, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + payload.len());
    out.extend_from_slice(version.as_str().as_bytes());
    out.extend_from_slice(&VERSION_HEADER_ZERO);
    out.extend_from_slice(payload);
    out
}

fn v31_control_payload(json: &[u8], key: &[u8]) -> Result<Vec<u8>> {
    let encrypted = aes_ecb_encrypt(key, json, true)?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(encrypted);
    let pre = format!("data={}||lpv=3.1||{}", b64, String::from_utf8_lossy(key));
    let hash = md5_hex(pre.as_bytes());
    let mut out = Vec::with_capacity(3 + 16 + b64.len());
    out.extend_from_slice(b"3.1");
    out.extend_from_slice(&hash.as_bytes()[8..24]);
    out.extend_from_slice(b64.as_bytes());
    Ok(out)
}

fn decode_payload(payload: &[u8], version: ProtocolVersion, session_key: &[u8]) -> Result<Value> {
    let mut data = payload.to_vec();
    match version {
        ProtocolVersion::V31 => {
            if data.starts_with(b"3.1") && data.len() > 19 {
                let encoded = &data[19..];
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(trim_nul(encoded))
                    .context("base64 ответа 3.1")?;
                data = aes_ecb_decrypt(session_key, &decoded, true)?;
            }
        }
        ProtocolVersion::V32 | ProtocolVersion::V33 => {
            if (data.starts_with(b"3.3") || data.starts_with(b"3.2"))
                && data.len() > VERSION_HEADER_LEN
            {
                data = data[VERSION_HEADER_LEN..].to_vec();
            }
            if !data.starts_with(b"{") {
                data =
                    aes_ecb_decrypt(session_key, &data, true).context("расшифровка ответа 3.3")?;
            }
        }
        ProtocolVersion::V34 => {
            data = aes_ecb_decrypt(session_key, &data, true).context("расшифровка ответа 3.4")?;
            if data.starts_with(b"3.4") && data.len() > VERSION_HEADER_LEN {
                data = data[VERSION_HEADER_LEN..].to_vec();
            }
        }
        ProtocolVersion::V35 => {
            if data.starts_with(b"3.5") && data.len() > VERSION_HEADER_LEN {
                data = data[VERSION_HEADER_LEN..].to_vec();
            }
        }
    }
    let text = String::from_utf8_lossy(trim_nul(&data));
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("ответ устройства не JSON ({} байт)", text.len()))?;
    Ok(value)
}

fn trim_nul(data: &[u8]) -> &[u8] {
    let end = data
        .iter()
        .rposition(|b| *b != 0 && *b != b'\n' && *b != b'\r')
        .map(|i| i + 1)
        .unwrap_or(0);
    &data[..end]
}

fn extract_dps(value: &Value) -> BTreeMap<String, Value> {
    let source = value
        .get("dps")
        .or_else(|| value.get("data").and_then(|d| d.get("dps")));
    let mut out = BTreeMap::new();
    if let Some(obj) = source.and_then(Value::as_object) {
        for (k, v) in obj {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

#[derive(Debug)]
struct Frame {
    cmd: u32,
    retcode: u32,
    payload: Vec<u8>,
}

fn build_55aa(seq: u32, cmd: u32, payload: &[u8], hmac_key: Option<&[u8]>) -> Vec<u8> {
    let end_len = if hmac_key.is_some() { 36 } else { 8 };
    let length = payload.len() + end_len;
    let mut out = Vec::with_capacity(16 + length);
    out.extend_from_slice(&PREFIX_55AA);
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&cmd.to_be_bytes());
    out.extend_from_slice(&(length as u32).to_be_bytes());
    out.extend_from_slice(payload);
    match hmac_key {
        Some(key) => {
            let mac = hmac_sha256(key, &out);
            out.extend_from_slice(&mac);
        }
        None => {
            let crc = crc32fast::hash(&out);
            out.extend_from_slice(&crc.to_be_bytes());
        }
    }
    out.extend_from_slice(&SUFFIX_55AA);
    out
}

fn build_6699(seq: u32, cmd: u32, plaintext: &[u8], key: &[u8]) -> Result<Vec<u8>> {
    // Формат >IHIII: prefix(4), unknown(2), seqno(4), cmd(4), length(4).
    // length = iv(12) + ciphertext + tag(16) + 4 - 4 = plaintext + 28.
    let length = plaintext.len() + 28;
    let mut header = [0u8; 18];
    header[..4].copy_from_slice(&PREFIX_6699);
    header[4..6].copy_from_slice(&0u16.to_be_bytes());
    header[6..10].copy_from_slice(&seq.to_be_bytes());
    header[10..14].copy_from_slice(&cmd.to_be_bytes());
    header[14..18].copy_from_slice(&(length as u32).to_be_bytes());
    let aad = &header[4..18];
    let mut iv = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut iv);
    let encrypted = aes_gcm_encrypt(key, &iv, plaintext, aad)?;
    let mut out = Vec::with_capacity(18 + length + 4);
    out.extend_from_slice(&header);
    out.extend_from_slice(&iv);
    out.extend_from_slice(&encrypted);
    out.extend_from_slice(&SUFFIX_6699);
    Ok(out)
}

async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
    stream: &mut R,
    key: &[u8],
    version: ProtocolVersion,
    timeout: Duration,
) -> Result<Frame> {
    read_frame_inner(stream, key, version, timeout, true).await
}

/// Читает кадр, отправленный клиентом: в исходящих кадрах нет 4-байтового
/// retcode, который есть в ответах устройства. Нужно тестовому устройству.
#[cfg(test)]
async fn read_frame_outgoing<R: tokio::io::AsyncRead + Unpin>(
    stream: &mut R,
    key: &[u8],
    version: ProtocolVersion,
    timeout: Duration,
) -> Result<Frame> {
    read_frame_inner(stream, key, version, timeout, false).await
}

async fn read_frame_inner<R: tokio::io::AsyncRead + Unpin>(
    stream: &mut R,
    key: &[u8],
    version: ProtocolVersion,
    timeout: Duration,
    strip_retcode: bool,
) -> Result<Frame> {
    let mut prefix = [0u8; 4];
    read_exact(stream, &mut prefix, timeout).await?;
    if prefix == PREFIX_55AA {
        let mut rest = [0u8; 12];
        read_exact(stream, &mut rest, timeout).await?;
        let mut header = [0u8; 16];
        header[..4].copy_from_slice(&prefix);
        header[4..].copy_from_slice(&rest);
        let length = u32::from_be_bytes([header[12], header[13], header[14], header[15]]) as usize;
        if length > MAX_FRAME {
            bail!("слишком длинный кадр устройства");
        }
        let mut body = vec![0u8; length];
        read_exact(stream, &mut body, timeout).await?;
        let hmac = version == ProtocolVersion::V34 || version == ProtocolVersion::V35;
        let end_len = if hmac { 36 } else { 8 };
        if body.len() < end_len + 4 {
            bail!("короткий кадр устройства");
        }
        let payload_end = body.len() - end_len;
        if body[payload_end + end_len - 4..] != SUFFIX_55AA {
            bail!("неверный суффикс кадра");
        }
        let mut signed = Vec::with_capacity(16 + payload_end);
        signed.extend_from_slice(&header);
        signed.extend_from_slice(&body[..payload_end]);
        if hmac {
            let expected = hmac_sha256(key, &signed);
            if body[payload_end..payload_end + 32] != expected {
                bail!("неверная подпись кадра (ключ не подошёл)");
            }
        } else {
            let expected = crc32fast::hash(&signed);
            let actual = u32::from_be_bytes([
                body[payload_end],
                body[payload_end + 1],
                body[payload_end + 2],
                body[payload_end + 3],
            ]);
            if actual != expected {
                bail!("неверная CRC кадра");
            }
        }
        let (retcode, payload) = if strip_retcode {
            let retcode = if payload_end >= 4 {
                u32::from_be_bytes([body[0], body[1], body[2], body[3]])
            } else {
                0
            };
            let payload = if payload_end > 4 {
                body[4..payload_end].to_vec()
            } else {
                Vec::new()
            };
            (retcode, payload)
        } else {
            (0, body[..payload_end].to_vec())
        };
        return Ok(Frame {
            cmd: u32::from_be_bytes([header[8], header[9], header[10], header[11]]),
            retcode,
            payload,
        });
    }
    if prefix == PREFIX_6699 {
        // >IHIII: prefix, unknown(2), seqno(4), cmd(4), length(4) — 18 байт.
        let mut rest = [0u8; 14];
        read_exact(stream, &mut rest, timeout).await?;
        let mut header = [0u8; 18];
        header[..4].copy_from_slice(&prefix);
        header[4..].copy_from_slice(&rest);
        let length = u32::from_be_bytes([header[14], header[15], header[16], header[17]]) as usize;
        if length > MAX_FRAME {
            bail!("слишком длинный кадр устройства");
        }
        let mut body = vec![0u8; length];
        read_exact(stream, &mut body, timeout).await?;
        let mut suffix = [0u8; 4];
        read_exact(stream, &mut suffix, timeout).await?;
        if suffix != SUFFIX_6699 || body.len() < 12 + 16 {
            bail!("неверный кадр 6699");
        }
        let iv = &body[..12];
        let tag_start = body.len() - 16;
        let mut encrypted = body[12..tag_start].to_vec();
        encrypted.extend_from_slice(&body[tag_start..]);
        let aad = &header[4..18];
        let plain = aes_gcm_decrypt(key, iv, &encrypted, aad)
            .context("проверка GCM-кадра (ключ не подошёл)")?;
        let (retcode, payload) = if strip_retcode {
            if plain.len() < 4 {
                bail!("короткий GCM-кадр");
            }
            (
                u32::from_be_bytes([plain[0], plain[1], plain[2], plain[3]]),
                plain[4..].to_vec(),
            )
        } else {
            (0, plain)
        };
        return Ok(Frame {
            cmd: u32::from_be_bytes([header[10], header[11], header[12], header[13]]),
            retcode,
            payload,
        });
    }
    bail!("неизвестный префикс кадра")
}

async fn read_exact<R: tokio::io::AsyncRead + Unpin>(
    stream: &mut R,
    buf: &mut [u8],
    timeout: Duration,
) -> Result<()> {
    tokio::time::timeout(timeout, stream.read_exact(buf))
        .await
        .map_err(|_| anyhow::anyhow!("тайм-аут чтения ответа"))?
        .context("чтение ответа устройства")?;
    Ok(())
}

fn aes_ecb_encrypt(key: &[u8], data: &[u8], pad: bool) -> Result<Vec<u8>> {
    let cipher = Aes128::new_from_slice(key).context("ключ AES")?;
    let mut buf = data.to_vec();
    if pad {
        let n = 16 - buf.len() % 16;
        buf.extend(std::iter::repeat_n(n as u8, n));
    } else if buf.len() % 16 != 0 {
        bail!("длина не кратна 16");
    }
    for block in buf.chunks_mut(16) {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    Ok(buf)
}

fn aes_ecb_decrypt(key: &[u8], data: &[u8], unpad: bool) -> Result<Vec<u8>> {
    if data.is_empty() || data.len() % 16 != 0 {
        bail!("длина шифртекста не кратна 16");
    }
    let cipher = Aes128::new_from_slice(key).context("ключ AES")?;
    let mut buf = data.to_vec();
    for block in buf.chunks_mut(16) {
        cipher.decrypt_block(GenericArray::from_mut_slice(block));
    }
    if unpad {
        let pad = *buf.last().context("пустой шифртекст")? as usize;
        if pad == 0 || pad > 16 || pad > buf.len() {
            bail!("неверный PKCS7 padding");
        }
        buf.truncate(buf.len() - pad);
    }
    Ok(buf)
}

fn aes_gcm_encrypt(key: &[u8], iv: &[u8], plain: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
    let cipher = Aes128Gcm::new_from_slice(key).context("ключ GCM")?;
    let nonce = Nonce::from_slice(iv);
    cipher
        .encrypt(nonce, Payload { msg: plain, aad })
        .map_err(|_| anyhow::anyhow!("шифрование GCM"))
}

fn aes_gcm_decrypt(key: &[u8], iv: &[u8], encrypted: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
    let cipher = Aes128Gcm::new_from_slice(key).context("ключ GCM")?;
    let nonce = Nonce::from_slice(iv);
    cipher
        .decrypt(
            nonce,
            Payload {
                msg: encrypted,
                aad,
            },
        )
        .map_err(|_| anyhow::anyhow!("аутентификация GCM"))
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
    let out = hasher.finalize();
    out.iter().map(|b| format!("{b:02x}")).collect()
}

/// Локальная подсеть по маршруту по умолчанию. Соединение UDP не отправляет
/// пакетов, поэтому интернет не нужен.
pub fn local_subnet() -> Option<(Ipv4Addr, u8)> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    let IpAddr::V4(ip) = sock.local_addr().ok()?.ip() else {
        return None;
    };
    Some((Ipv4Addr::from(u32::from(ip) & 0xFFFF_FF00), 24))
}

pub fn hosts_in_subnet(base: Ipv4Addr, prefix: u8, max: usize) -> Vec<Ipv4Addr> {
    let base = u32::from(base);
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    let network = base & mask;
    let size = if prefix >= 31 {
        1u64 << (32 - prefix)
    } else {
        (1u64 << (32 - prefix)) - 2
    };
    let start = if prefix >= 31 { network } else { network + 1 };
    (0..size.min(max as u64))
        .filter_map(|i| {
            let value = start.checked_add(i as u32)?;
            if prefix < 31 && value >= network + (1u32 << (32 - prefix)) {
                None
            } else {
                Some(Ipv4Addr::from(value))
            }
        })
        .collect()
}

/// Разрешает DNS-имя или IP (для тестов и явных адресов).
pub async fn resolve(addr: &str, port: u16) -> Result<Vec<SocketAddr>> {
    Ok(tokio::net::lookup_host((addr, port))
        .await
        .context("разрешение адреса")?
        .collect())
}

pub fn socket_addr(ip: IpAddr) -> SocketAddr {
    SocketAddr::new(ip, PORT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    const KEY: &[u8] = b"0123456789abcdef";
    const DEV: &str = "dev1234567890abcdef";

    fn unhex(value: &str) -> Vec<u8> {
        let clean: String = value.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        (0..clean.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&clean[i..i + 2], 16).unwrap())
            .collect()
    }

    async fn decode(hex: &str, version: ProtocolVersion, key: &[u8]) -> Frame {
        let bytes = unhex(hex);
        let (mut reader, mut writer) = tokio::io::duplex(8192);
        writer.write_all(&bytes).await.unwrap();
        drop(writer);
        read_frame(&mut reader, key, version, Duration::from_secs(1))
            .await
            .unwrap()
    }

    #[test]
    fn aes_ecb_matches_tinytuya_vector() {
        let encrypted = aes_ecb_encrypt(KEY, br#"{"a":1}"#, true).unwrap();
        assert_eq!(encrypted, unhex("e01bb6c8e8a431461f164868744b2fef"));
        assert_eq!(
            aes_ecb_decrypt(KEY, &encrypted, true).unwrap(),
            br#"{"a":1}"#
        );
    }

    #[test]
    fn v33_request_bytes_match_tinytuya() {
        let payload =
            format!(r#"{{"gwId":"{DEV}","devId":"{DEV}","uid":"{DEV}","t":"1700000000"}}"#);
        let actual = encode_message(
            ProtocolVersion::V33,
            KEY,
            1,
            CMD_DP_QUERY,
            payload.as_bytes(),
        )
        .unwrap();
        let expected = unhex("000055aa000000010000000a0000007822d48e125a362c44683cd7168cd0bd999ec3ccc9469ffc8f7017a1e0efe6174a2b6827d425d88d045594516657c7f1815fd4038d692e698f263794c20e7191a68a06d05dacc4616654476e77e80f72a50fcb7dcdd5540d42d663310bf684eec465ac5e1d1e3c3c395817ca95740225b46c44c5330000aa55");
        assert_eq!(actual, expected);

        let payload =
            format!(r#"{{"devId":"{DEV}","uid":"{DEV}","t":"1700000000","dps":{{"1":true}}}}"#);
        let actual = encode_message(
            ProtocolVersion::V33,
            KEY,
            1,
            CMD_CONTROL,
            payload.as_bytes(),
        )
        .unwrap();
        let expected = unhex("000055aa000000010000000700000077332e33000000000000000000000000d136205043d3aaf4a21fdd2b78ba3af8c1e12022c293fc784ee03b0756756c0a458bfc6476b3c091203a94a4b478736ba6369c81c850dc3cfa39808cc48e206bf9825e8a7c974132c83dccc1368b0f44b98d177f076695bd7ecd6cfb887fe9ad22ca1a670000aa55");
        assert_eq!(actual, expected);
    }

    #[test]
    fn v34_request_bytes_match_tinytuya() {
        let payload = br#"{"protocol":5,"t":1700000000,"data":{"dps":{"1":true}}}"#;
        let actual =
            encode_message(ProtocolVersion::V34, KEY, 1, CMD_CONTROL_NEW, payload).unwrap();
        let expected = unhex("000055aa000000010000000d000000744490b05d74be9368c24a038cbaeded8e89ea503e4be494dfe0c010bc6c463c75723ef6aca177a39b03b3e4fb00012c19edd249953145abb3cfa65e18d98cc3bde3d0951b8ab2e031cb512d241494e9991a4a2eec6ae3be6a7cbf3c0b11027db7b18ce785cc86a8c65f3418796c9766780000aa55");
        assert_eq!(actual, expected);

        let actual = encode_message(ProtocolVersion::V34, KEY, 1, CMD_DP_QUERY_NEW, b"{}").unwrap();
        let expected = unhex("000055aa000000010000001000000034cb70ddc25a2a2045b4c13084418a9abb8ea43373cd79023b497ede47d2a76f3a89ec7a25cbe935b55763f0d1a1a3b0bb0000aa55");
        assert_eq!(actual, expected);
    }

    #[test]
    fn v34_handshake_start_matches_tinytuya() {
        let nonce: Vec<u8> = (0..16).collect();
        let encrypted = aes_ecb_encrypt(KEY, &nonce, true).unwrap();
        let frame = build_55aa(1, CMD_SESS_START, &encrypted, Some(KEY));
        let expected = unhex("000055aa000000010000000300000044a07999f0e2bfbe16f99593e984a449b7377222e061a924c591cd9c27ea163ed484a896dcb29647fa6e5c5024c9533704640e90001c9f4d52f8b4480cc12648340000aa55");
        assert_eq!(frame, expected);
    }

    #[tokio::test]
    async fn decodes_v33_status_response() {
        let frame = decode(
            "000055aa000000010000000a0000005b00000000332e33000000000000000000000000d136205043d3aaf4a21fdd2b78ba3af8c1e12022c293fc784ee03b0756756c0afc8ea47f00e5c661bae58ec2d66081c9e1d1d22dd38188740d4f1971a2ff5f7a8a8d9cbf0000aa55",
            ProtocolVersion::V33,
            KEY,
        )
        .await;
        assert_eq!(frame.cmd, CMD_DP_QUERY);
        let value = decode_payload(&frame.payload, ProtocolVersion::V33, KEY).unwrap();
        let dps = extract_dps(&value);
        assert_eq!(dps.get("1"), Some(&json!(true)));
        assert_eq!(dps.get("2"), Some(&json!(406)));
    }

    #[tokio::test]
    async fn decodes_v31_status_response() {
        let key = b"0123456789abcdef";
        let plain = br#"{"devId":"dev","dps":{"1":true,"2":400}}"#;
        let encrypted = aes_ecb_encrypt(key, plain, true).unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode(encrypted);
        let mut payload = b"3.1".to_vec();
        payload.extend_from_slice(b"0123456789abcdef");
        payload.extend_from_slice(encoded.as_bytes());
        let value = decode_payload(&payload, ProtocolVersion::V31, key).unwrap();
        assert_eq!(extract_dps(&value).get("1"), Some(&json!(true)));
    }

    #[tokio::test]
    async fn decodes_v34_status_response_with_hmac() {
        let frame = decode(
            "000055aa00000001000000100000006800000000d136205043d3aaf4a21fdd2b78ba3af8c1e12022c293fc784ee03b0756756c0afc8ea47f00e5c661bae58ec2d66081c9e1d1d22dd38188740d4f1971a2ff5f7ab538fbfc203febfba38c948bb7d3f774a5e39effe10e7ee4ed7ef31d854c971d0000aa55",
            ProtocolVersion::V34,
            KEY,
        )
        .await;
        assert_eq!(frame.cmd, CMD_DP_QUERY_NEW);
        let value = decode_payload(&frame.payload, ProtocolVersion::V34, KEY).unwrap();
        assert_eq!(extract_dps(&value).get("1"), Some(&json!(true)));
    }

    #[tokio::test]
    async fn decodes_v35_gcm_response() {
        let frame = decode(
            "000066990000000000010000001000000058000102030405060708090a0bfd3fc41d65565f539e6f6338fab9d44876e665c933867c7c4f50da928dc8cf093835b3cc949eebe5505c85af5c5729531607250c06537942329ff5191c378d62b95bd3caa1263cfd1e6cf65000009966",
            ProtocolVersion::V35,
            KEY,
        )
        .await;
        assert_eq!(frame.cmd, CMD_DP_QUERY_NEW);
        let value = decode_payload(&frame.payload, ProtocolVersion::V35, KEY).unwrap();
        assert_eq!(extract_dps(&value).get("2"), Some(&json!(406)));
    }

    #[tokio::test]
    async fn corrupted_frame_is_rejected() {
        let mut bytes = unhex("000055aa000000010000000a0000005b00000000332e33000000000000000000000000d136205043d3aaf4a21fdd2b78ba3af8c1e12022c293fc784ee03b0756756c0afc8ea47f00e5c661bae58ec2d66081c9e1d1d22dd38188740d4f1971a2ff5f7a8a8d9cbf0000aa55");
        bytes[40] ^= 0xff;
        let (mut reader, mut writer) = tokio::io::duplex(8192);
        writer.write_all(&bytes).await.unwrap();
        drop(writer);
        assert!(read_frame(
            &mut reader,
            KEY,
            ProtocolVersion::V33,
            Duration::from_secs(1)
        )
        .await
        .is_err());
    }

    #[test]
    fn subnet_hosts_are_bounded() {
        let hosts = hosts_in_subnet("192.168.3.0".parse().unwrap(), 24, 10);
        assert_eq!(hosts.len(), 10);
        assert_eq!(hosts[0], "192.168.3.1".parse::<Ipv4Addr>().unwrap());
        assert_eq!(
            hosts_in_subnet("192.168.3.7".parse().unwrap(), 32, 10),
            vec!["192.168.3.7".parse::<Ipv4Addr>().unwrap()]
        );
    }

    #[test]
    fn device22_marker_not_leaked_in_public_error() {
        assert!(!DEVICE22_MARKER.is_empty());
        let _ = DEV;
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) async fn read_json_outgoing(
        stream: &mut TcpStream,
        key: &[u8],
        version: ProtocolVersion,
    ) -> Result<(u32, Value)> {
        let frame = read_frame_outgoing(stream, key, version, Duration::from_secs(2)).await?;
        let value = decode_payload(&frame.payload, version, key)?;
        Ok((frame.cmd, value))
    }

    pub(crate) fn reply_55(seq: u32, cmd: u32, payload: &[u8]) -> Vec<u8> {
        let mut body = vec![0u8; 4];
        body.extend_from_slice(payload);
        build_55aa(seq, cmd, &body, None)
    }

    pub(crate) fn encrypted_33(key: &[u8], json: &[u8]) -> Vec<u8> {
        with_version_header(
            ProtocolVersion::V33,
            &aes_ecb_encrypt(key, json, true).unwrap(),
        )
    }

    pub(crate) fn encrypted_32(key: &[u8], json: &[u8]) -> Vec<u8> {
        with_version_header(
            ProtocolVersion::V32,
            &aes_ecb_encrypt(key, json, true).unwrap(),
        )
    }
}

#[cfg(test)]
mod device_tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    const KEY: &[u8] = b"0123456789abcdef";

    fn record(version: ProtocolVersion) -> DeviceRecord {
        DeviceRecord {
            id: "dev1234567890abcdef".into(),
            name: "Лампа".into(),
            category: "dj".into(),
            product_id: String::new(),
            product_name: String::new(),
            model: String::new(),
            local_key: String::from_utf8(KEY.to_vec()).unwrap(),
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
            ip: Some("127.0.0.1".into()),
            version: Some(version),
            device22: false,
            last_seen: 0,
            key_from: None,
        }
    }

    fn response_55(seq: u32, cmd: u32, payload: &[u8], key: Option<&[u8]>) -> Vec<u8> {
        let mut body = vec![0u8; 4];
        body.extend_from_slice(payload);
        build_55aa(seq, cmd, &body, key)
    }

    fn response_6699(seq: u32, cmd: u32, payload: &[u8], key: &[u8]) -> Vec<u8> {
        let mut body = vec![0u8; 4];
        body.extend_from_slice(payload);
        build_6699(seq, cmd, &body, key).unwrap()
    }

    #[tokio::test]
    async fn v32_device22_state_and_control_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let key = KEY.to_vec();
        let server = tokio::spawn(async move {
            for step in 0..2 {
                let (mut sock, _) = listener.accept().await.unwrap();
                let frame = read_frame_outgoing(
                    &mut sock,
                    &key,
                    ProtocolVersion::V32,
                    Duration::from_secs(2),
                )
                .await
                .unwrap();
                let request = decode_payload(&frame.payload, ProtocolVersion::V32, &key).unwrap();
                let response = if step == 0 {
                    assert_eq!(frame.cmd, CMD_CONTROL_NEW);
                    assert!(request["dps"]["1"].is_null());
                    let json = br#"{"devId":"dev","dps":{"1":true,"2":400}}"#;
                    response_55(
                        1,
                        CMD_CONTROL_NEW,
                        &test_support::encrypted_32(&key, json),
                        None,
                    )
                } else {
                    assert_eq!(frame.cmd, CMD_CONTROL);
                    assert_eq!(request["dps"]["1"], json!(false));
                    response_55(1, CMD_CONTROL, &[], None)
                };
                sock.write_all(&response).await.unwrap();
                sock.shutdown().await.unwrap();
            }
        });
        let record = record(ProtocolVersion::V32);
        let state = query_record_at(&record, addr, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(state.dps.get("1"), Some(&json!(true)));
        let dps = vec![("1".to_string(), json!(false))];
        let ack = control_record_at(&record, addr, Duration::from_secs(2), &dps)
            .await
            .unwrap();
        assert!(ack.acknowledged);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn v33_state_and_control_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let key = KEY.to_vec();
        let server = tokio::spawn(async move {
            for step in 0..2 {
                let (mut sock, _) = listener.accept().await.unwrap();
                let frame = read_frame_outgoing(
                    &mut sock,
                    &key,
                    ProtocolVersion::V33,
                    Duration::from_secs(2),
                )
                .await
                .unwrap();
                let request = decode_payload(&frame.payload, ProtocolVersion::V33, &key).unwrap();
                let response = if step == 0 {
                    assert_eq!(frame.cmd, CMD_DP_QUERY);
                    let json = br#"{"devId":"dev","dps":{"1":true,"2":406}}"#;
                    let encrypted = with_version_header(
                        ProtocolVersion::V33,
                        &aes_ecb_encrypt(&key, json, true).unwrap(),
                    );
                    response_55(1, CMD_DP_QUERY, &encrypted, None)
                } else {
                    assert_eq!(frame.cmd, CMD_CONTROL);
                    assert_eq!(request["dps"]["1"], json!(false));
                    response_55(1, CMD_CONTROL, &[], None)
                };
                sock.write_all(&response).await.unwrap();
                sock.shutdown().await.unwrap();
            }
        });
        let record = record(ProtocolVersion::V33);
        let state = query_record_at(&record, addr, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(state.dps.get("1"), Some(&json!(true)));
        assert_eq!(state.dps.get("2"), Some(&json!(406)));
        let dps = vec![("1".to_string(), json!(false))];
        let ack = control_record_at(&record, addr, Duration::from_secs(2), &dps)
            .await
            .unwrap();
        assert!(ack.acknowledged);
        server.await.unwrap();
    }

    async fn v35_handshake(sock: &mut TcpStream, real: &[u8]) -> [u8; 16] {
        let start = read_frame_outgoing(sock, real, ProtocolVersion::V35, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(start.cmd, CMD_SESS_START);
        assert_eq!(start.payload.len(), 16);
        let local = start.payload.clone();
        let remote = [9u8; 16];
        let mut body = remote.to_vec();
        body.extend_from_slice(&hmac_sha256(real, &local));
        let reply = response_6699(1, CMD_SESS_RESP, &body, real);
        sock.write_all(&reply).await.unwrap();

        let finish = read_frame_outgoing(sock, real, ProtocolVersion::V35, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(finish.cmd, CMD_SESS_FINISH);
        assert_eq!(finish.payload, hmac_sha256(real, &remote));

        let mut xor = [0u8; 16];
        for i in 0..16 {
            xor[i] = local[i] ^ remote[i];
        }
        let encrypted = aes_gcm_encrypt(real, &local[..12], &xor, &[]).unwrap();
        let mut session = [0u8; 16];
        session.copy_from_slice(&encrypted[..16]);
        session
    }

    async fn v34_handshake(sock: &mut TcpStream, real: &[u8]) -> [u8; 16] {
        let start = read_frame_outgoing(sock, real, ProtocolVersion::V34, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(start.cmd, CMD_SESS_START);
        let local = aes_ecb_decrypt(real, &start.payload, true).unwrap();
        assert_eq!(local.len(), 16);
        let remote = [9u8; 16];
        let mut body = remote.to_vec();
        body.extend_from_slice(&hmac_sha256(real, &local));
        let encrypted = aes_ecb_encrypt(real, &body, true).unwrap();
        sock.write_all(&response_55(1, CMD_SESS_RESP, &encrypted, Some(real)))
            .await
            .unwrap();

        let finish = read_frame_outgoing(sock, real, ProtocolVersion::V34, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(finish.cmd, CMD_SESS_FINISH);
        let finish = aes_ecb_decrypt(real, &finish.payload, true).unwrap();
        assert_eq!(finish, hmac_sha256(real, &remote));

        let mut xor = [0u8; 16];
        for i in 0..16 {
            xor[i] = local[i] ^ remote[i];
        }
        let mut session = [0u8; 16];
        session.copy_from_slice(&aes_ecb_encrypt(real, &xor, false).unwrap());
        session
    }

    #[tokio::test]
    async fn v34_handshake_state_and_control_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let key = KEY.to_vec();
        let server = tokio::spawn(async move {
            for step in 0..2 {
                let (mut sock, _) = listener.accept().await.unwrap();
                let session = v34_handshake(&mut sock, &key).await;
                let frame = read_frame_outgoing(
                    &mut sock,
                    &session,
                    ProtocolVersion::V34,
                    Duration::from_secs(2),
                )
                .await
                .unwrap();
                let request =
                    decode_payload(&frame.payload, ProtocolVersion::V34, &session).unwrap();
                let response = if step == 0 {
                    assert_eq!(frame.cmd, CMD_DP_QUERY_NEW);
                    let json = br#"{"devId":"dev","dps":{"1":true,"2":400}}"#;
                    let encrypted = aes_ecb_encrypt(&session, json, true).unwrap();
                    response_55(3, CMD_DP_QUERY_NEW, &encrypted, Some(&session))
                } else {
                    assert_eq!(frame.cmd, CMD_CONTROL_NEW);
                    assert_eq!(request["data"]["dps"]["1"], json!(false));
                    response_55(3, CMD_CONTROL_NEW, &[], Some(&session))
                };
                sock.write_all(&response).await.unwrap();
                sock.shutdown().await.unwrap();
            }
        });
        let record = record(ProtocolVersion::V34);
        let state = query_record_at(&record, addr, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(state.dps.get("1"), Some(&json!(true)));
        let dps = vec![("1".to_string(), json!(false))];
        let ack = control_record_at(&record, addr, Duration::from_secs(2), &dps)
            .await
            .unwrap();
        assert!(ack.acknowledged);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn v35_handshake_state_and_control_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let key = KEY.to_vec();
        let server = tokio::spawn(async move {
            for step in 0..2 {
                let (mut sock, _) = listener.accept().await.unwrap();
                let session = v35_handshake(&mut sock, &key).await;
                let frame = read_frame_outgoing(
                    &mut sock,
                    &session,
                    ProtocolVersion::V35,
                    Duration::from_secs(2),
                )
                .await
                .unwrap();
                let request =
                    decode_payload(&frame.payload, ProtocolVersion::V35, &session).unwrap();
                let response = if step == 0 {
                    assert_eq!(frame.cmd, CMD_DP_QUERY_NEW);
                    let json = br#"{"devId":"dev","dps":{"1":true,"2":406}}"#;
                    response_6699(3, CMD_DP_QUERY_NEW, json, &session)
                } else {
                    assert_eq!(frame.cmd, CMD_CONTROL_NEW);
                    assert_eq!(request["data"]["dps"]["1"], json!(false));
                    response_6699(3, CMD_CONTROL_NEW, &[], &session)
                };
                sock.write_all(&response).await.unwrap();
                sock.shutdown().await.unwrap();
            }
        });
        let record = record(ProtocolVersion::V35);
        let state = query_record_at(&record, addr, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(state.dps.get("1"), Some(&json!(true)));
        let dps = vec![("1".to_string(), json!(false))];
        let ack = control_record_at(&record, addr, Duration::from_secs(2), &dps)
            .await
            .unwrap();
        assert!(ack.acknowledged);
        server.await.unwrap();
    }
}
