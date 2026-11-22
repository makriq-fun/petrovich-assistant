//! Типы интеграции Tuya: устройства, DP-спецификации, протокол.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Версия локального протокола Tuya, которую понимает устройство.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProtocolVersion {
    #[serde(rename = "3.1")]
    V31,
    #[serde(rename = "3.2")]
    V32,
    #[serde(rename = "3.3")]
    V33,
    #[serde(rename = "3.4")]
    V34,
    #[serde(rename = "3.5")]
    V35,
}

impl ProtocolVersion {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V31 => "3.1",
            Self::V32 => "3.2",
            Self::V33 => "3.3",
            Self::V34 => "3.4",
            Self::V35 => "3.5",
        }
    }

    /// Порядок проверки: сначала массовый 3.3, затем шифрованные 3.4/3.5.
    pub fn probe_order(allow_v31: bool) -> &'static [ProtocolVersion] {
        const WITH_31: &[ProtocolVersion] = &[
            ProtocolVersion::V33,
            ProtocolVersion::V34,
            ProtocolVersion::V35,
            ProtocolVersion::V32,
            ProtocolVersion::V31,
        ];
        const WITHOUT_31: &[ProtocolVersion] = &[
            ProtocolVersion::V33,
            ProtocolVersion::V34,
            ProtocolVersion::V35,
            ProtocolVersion::V32,
        ];
        if allow_v31 {
            WITH_31
        } else {
            WITHOUT_31
        }
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ProtocolVersion {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value.trim() {
            "3.1" => Self::V31,
            "3.2" => Self::V32,
            "3.3" => Self::V33,
            "3.4" => Self::V34,
            "3.5" => Self::V35,
            _ => return Err(()),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Room {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
}

/// Преобразованный `statusFormat` из облака.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DpFormat {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub scale: Option<i32>,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub step: Option<f64>,
    #[serde(default)]
    pub unit: String,
    #[serde(default)]
    pub values: Vec<String>,
}

/// Связь DP-идентификатора устройства с кодом функции из облака.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DpInfo {
    pub dp_id: String,
    pub code: String,
    #[serde(default)]
    pub format: DpFormat,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FunctionSpec {
    pub code: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub values: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatusSpec {
    pub code: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub values: Value,
}

/// Устройство, как его отдаёт облако Tuya плюс локальные поля.
///
/// `Debug` намеренно не выводит `local_key` и `id`: ключ не должен попадать
/// в журналы даже при отладочной печати.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub product_id: String,
    #[serde(default)]
    pub product_name: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub local_key: String,
    #[serde(default)]
    pub sub: bool,
    /// Шлюз, если облако его сообщило.
    #[serde(default)]
    pub gateway_id: Option<String>,
    /// Идентификатор дочернего устройства в кадре шлюза.
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub online: bool,
    #[serde(default)]
    pub room: Option<Room>,
    #[serde(default)]
    pub home_id: String,
    #[serde(default)]
    pub home_name: String,
    #[serde(default)]
    pub functions: Vec<FunctionSpec>,
    #[serde(default)]
    pub status: Vec<StatusSpec>,
    /// dpId -> код функции. Пусто, если облако не разрешает локальный доступ.
    #[serde(default)]
    pub dp_map: BTreeMap<String, DpInfo>,
    /// Значения, полученные из облака при последнем обновлении.
    #[serde(default)]
    pub cloud_status: BTreeMap<String, Value>,
    #[serde(default)]
    pub local_supported: bool,
    #[serde(default)]
    pub unsupported_reason: String,
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub version: Option<ProtocolVersion>,
    /// Устройство требует команду 0x0d для чтения статуса.
    #[serde(default)]
    pub device22: bool,
    #[serde(default)]
    pub last_seen: i64,
    /// Чей ключ подошёл к шлюзу (для Zigbee-устройств).
    #[serde(default)]
    pub key_from: Option<String>,
}

impl fmt::Debug for DeviceRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceRecord")
            .field("name", &self.name)
            .field("category", &self.category)
            .field("room", &self.room.as_ref().map(|r| &r.name))
            .field("sub", &self.sub)
            .field("online", &self.online)
            .field("local_supported", &self.local_supported)
            .field("ip", &self.ip)
            .field("version", &self.version)
            .field("device22", &self.device22)
            .finish_non_exhaustive()
    }
}

impl DeviceRecord {
    pub fn is_usable_local(&self) -> bool {
        self.local_supported
            && !self.local_key.is_empty()
            && !self.dp_map.is_empty()
            && self.unsupported_reason.is_empty()
    }
}

/// Категория политики безопасности. Вычисляется только по `category`
/// облака, а не по названию или тексту команды.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CategoryPolicy {
    Control(DeviceKind),
    ReadOnly(DeviceKind),
    Forbidden,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeviceKind {
    Light,
    Plug,
    Switch,
    Climate,
    Cover,
    Sensor,
    Hub,
}

impl DeviceKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Light => "свет",
            Self::Plug => "розетка",
            Self::Switch => "выключатель",
            Self::Climate => "климат",
            Self::Cover => "шторы",
            Self::Sensor => "датчик",
            Self::Hub => "шлюз",
        }
    }

    pub fn can_control(self) -> bool {
        !matches!(self, Self::Sensor | Self::Hub)
    }
}

/// Один DP, которым разрешено управлять.
#[derive(Clone, Debug, PartialEq)]
pub struct DpRef {
    pub dp_id: String,
    pub code: String,
    pub format: DpFormat,
}

/// Белый список функций конкретного устройства. Никакие другие DP не
/// отправляются: планировщик команд работает только с этими полями.
#[derive(Clone, Debug, Default)]
pub struct Capabilities {
    pub power: Vec<DpRef>,
    pub brightness: Option<DpRef>,
    pub color_temp: Option<DpRef>,
    pub color: Option<DpRef>,
    pub mode: Option<DpRef>,
    pub temp_set: Option<DpRef>,
    pub temp_current: Option<DpRef>,
    pub fan: Option<DpRef>,
    pub position: Option<DpRef>,
    pub cover_control: Option<DpRef>,
}

impl Capabilities {
    pub fn has_control(&self) -> bool {
        !self.power.is_empty()
            || self.brightness.is_some()
            || self.color_temp.is_some()
            || self.color.is_some()
            || self.mode.is_some()
            || self.temp_set.is_some()
            || self.fan.is_some()
            || self.position.is_some()
    }

    pub fn feature_labels(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.power.is_empty() {
            out.push("включение");
        }
        if self.brightness.is_some() {
            out.push("яркость");
        }
        if self.color_temp.is_some() {
            out.push("температура света");
        }
        if self.color.is_some() {
            out.push("цвет");
        }
        if self.mode.is_some() {
            out.push("режим");
        }
        if self.temp_set.is_some() {
            out.push("температура");
        }
        if self.fan.is_some() {
            out.push("вентилятор");
        }
        if self.position.is_some() {
            out.push("положение");
        }
        out
    }
}
