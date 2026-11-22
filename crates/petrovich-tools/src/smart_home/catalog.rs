//! Политика безопасности, возможности устройств и планирование DP-команд.
//!
//! Здесь нет ни одного пути «отправить произвольный DP»: команда строится
//! только из белого списка функций, полученных из облака, и только для
//! категорий, разрешённых кодом ниже.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::model::{
    Capabilities, CategoryPolicy, DeviceKind, DeviceRecord, DpFormat, DpInfo, DpRef,
};

/// Категории, которые разрешено включать, выключать и регулировать.
/// Список намеренно короткий: всё остальное — запрещено или неизвестно.
pub fn policy_for(category: &str) -> CategoryPolicy {
    let category = category.trim().to_ascii_lowercase();
    if is_critical_category(&category) {
        return CategoryPolicy::Forbidden;
    }
    match category.as_str() {
        // Освещение.
        "dj" | "dd" | "xdd" | "fwd" | "tgq" | "tyndj" | "sxd" | "qjd" | "mbd" | "gyd" | "jcd"
        | "tdq" | "dgy" => CategoryPolicy::Control(DeviceKind::Light),
        // Розетки, удлинители, настенные выключатели.
        "cz" | "pc" | "kg" | "kgh" => CategoryPolicy::Control(DeviceKind::Plug),
        // Климатическая техника: кондиционеры, обогреватели, тёплый пол.
        "kt" | "ktk" | "ntq" | "wk" | "rs" | "qn" | "hw" | "sn" | "ktq" => {
            CategoryPolicy::Control(DeviceKind::Climate)
        }
        // Шторы и жалюзи: только положение, без сценариев и датчиков.
        "cl" | "clkg" => CategoryPolicy::Control(DeviceKind::Cover),
        // Только чтение: температура, влажность, освещённость.
        "wsdcg" | "wsd" | "ldcg" | "hwsd" | "pir" | "mcs" | "zd" | "sj" | "hps" | "ylcg" => {
            CategoryPolicy::ReadOnly(DeviceKind::Sensor)
        }
        // Шлюзы: инфраструктура для дочерних устройств, не цель управления.
        "wg" | "wg2" | "wgsxj" | "lyqwg" | "bywg" | "zigbee" | "dgnzk" | "videohub" | "xnwg"
        | "qtyycp" | "alexa_yywg" | "gywg" | "cnwg" | "wfcon" => {
            CategoryPolicy::ReadOnly(DeviceKind::Hub)
        }
        _ => CategoryPolicy::Unknown,
    }
}

/// Категории, которые нельзя даже показывать инструменту. Явный список
/// нужен только для понятного отказа; всё неизвестное и так запрещено.
pub fn is_critical_category(category: &str) -> bool {
    matches!(
        category.trim().to_ascii_lowercase().as_str(),
        "ms" | "mk"
            | "lock"
            | "gyms"
            | "hotelms"
            | "jtmsbh"
            | "jtmspro"
            | "photolock"
            | "videolock"
            | "wireless_batterylock"
            | "jkm"
            | "sp"
            | "sos"
            | "ywbj"
            | "rqbj"
            | "jwbj"
            | "fgbj"
            | "aqbj"
            | "sgbj"
            | "dgnbj"
            | "alarm"
            | "ipc"
            | "camera"
            | "doorbell"
            | "db"
            | "ckmk"
            | "ckmkzq"
            | "gmk"
    )
}

/// Собирает разрешённые функции устройства. Неизвестный код не попадает в
/// результат, даже если он есть в облаке.
pub fn capabilities(record: &DeviceRecord) -> Result<Capabilities, String> {
    if !record.local_supported {
        return Err(if record.unsupported_reason.is_empty() {
            "облако не разрешает локальное управление".into()
        } else {
            record.unsupported_reason.clone()
        });
    }
    if record.local_key.len() != 16 {
        return Err("локальный ключ отсутствует или неверной длины".into());
    }
    if record.dp_map.is_empty() {
        return Err("облако не сообщило карту DP для локального управления".into());
    }
    let policy = policy_for(&record.category);
    let kind = match policy {
        CategoryPolicy::Control(k) | CategoryPolicy::ReadOnly(k) => k,
        CategoryPolicy::Forbidden => return Err("категория запрещена политикой".into()),
        CategoryPolicy::Unknown => {
            return Err("неизвестная категория устройства заблокирована".into())
        }
    };

    let by_code = code_map(record);
    let mut caps = Capabilities::default();
    let mut used = std::collections::BTreeSet::new();

    if kind.can_control() || matches!(kind, DeviceKind::Climate) {
        caps.power = power_refs(record, &by_code);
        for dp in &caps.power {
            used.insert(dp.dp_id.clone());
        }
        caps.brightness = take_code(
            &by_code,
            &mut used,
            &["bright_value_v2", "bright_value", "brightness"],
        );
        caps.color_temp = take_code(
            &by_code,
            &mut used,
            &["temp_value_v2", "temp_value", "colour_temp", "color_temp"],
        );
        caps.color = take_code(
            &by_code,
            &mut used,
            &["colour_data_v2", "colour_data", "color"],
        );
        caps.mode = match kind {
            DeviceKind::Climate => {
                take_code(&by_code, &mut used, &["mode", "work_mode", "mode_enum"])
            }
            _ => take_code(&by_code, &mut used, &["work_mode", "mode"]),
        };
        caps.temp_set = take_code(&by_code, &mut used, &["temp_set", "temp_set_f"]);
        caps.temp_current = take_code(&by_code, &mut used, &["temp_current", "temp_current_f"]);
        caps.fan = take_code(
            &by_code,
            &mut used,
            &["fan_speed_enum", "fan_speed", "wind_speed", "fan_speed_1"],
        );
        caps.position = take_code(&by_code, &mut used, &["percent_control", "position"]);
        caps.cover_control = take_code(&by_code, &mut used, &["control", "control_1"]);
    }

    for required in [
        caps.brightness.as_ref(),
        caps.color_temp.as_ref(),
        caps.color.as_ref(),
        caps.temp_set.as_ref(),
        caps.fan.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        validate_format(required)?;
    }
    Ok(caps)
}

fn take_code(
    by_code: &BTreeMap<String, DpRef>,
    used: &mut std::collections::BTreeSet<String>,
    codes: &[&str],
) -> Option<DpRef> {
    for code in codes {
        if let Some(dp) = by_code.get(*code) {
            if used.insert(dp.dp_id.clone()) {
                return Some(dp.clone());
            }
        }
    }
    None
}

fn code_map(record: &DeviceRecord) -> BTreeMap<String, DpRef> {
    let mut by_code = BTreeMap::new();
    for (dp_id, info) in &record.dp_map {
        let format = effective_format(record, info);
        by_code.insert(
            info.code.clone(),
            DpRef {
                dp_id: dp_id.clone(),
                code: info.code.clone(),
                format,
            },
        );
    }
    by_code
}

fn effective_format(record: &DeviceRecord, info: &DpInfo) -> DpFormat {
    let mut format = info.format.clone();
    if format.kind.is_empty() {
        if let Some(f) = record.functions.iter().find(|f| f.code == info.code) {
            format.kind = f.kind.clone();
            merge_values(&mut format, &f.values);
        } else if let Some(s) = record.status.iter().find(|s| s.code == info.code) {
            format.kind = s.kind.clone();
            merge_values(&mut format, &s.values);
        }
    }
    if format.kind.is_empty() {
        // statusFormat из облака обычно содержит type.
        format.kind = match (format.min, format.max, format.values.is_empty()) {
            (Some(_), Some(_), _) => "Integer".into(),
            (_, _, false) => "Enum".into(),
            _ => String::new(),
        };
    }
    format
}

/// Разбирает `values` функции/статуса: диапазон или перечисление.
pub fn merge_values(format: &mut DpFormat, values: &Value) {
    let Some(obj) = values.as_object() else {
        if let Some(arr) = values.as_array() {
            format.values = arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
        }
        return;
    };
    if format.min.is_none() {
        format.min = obj.get("min").and_then(Value::as_f64);
    }
    if format.max.is_none() {
        format.max = obj.get("max").and_then(Value::as_f64);
    }
    if format.scale.is_none() {
        format.scale = obj
            .get("scale")
            .and_then(Value::as_i64)
            .and_then(|v| i32::try_from(v).ok());
    }
    if format.step.is_none() {
        format.step = obj.get("step").and_then(Value::as_f64);
    }
    if format.unit.is_empty() {
        format.unit = obj
            .get("unit")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
    }
    if format.values.is_empty() {
        if let Some(arr) = obj.get("range").and_then(Value::as_array) {
            format.values = arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
        }
    }
}

fn validate_format(dp: &DpRef) -> Result<(), String> {
    match dp.format.kind.as_str() {
        "Integer" | "Bool" | "Boolean" => {
            if dp.format.min.is_none() || dp.format.max.is_none() {
                return Err(format!("у функции {} нет диапазона значений", dp.code));
            }
        }
        "Enum" => {
            if dp.format.values.is_empty() {
                return Err(format!(
                    "у перечисления {} нет допустимых значений",
                    dp.code
                ));
            }
        }
        "Json" | "String" => {}
        _ => {}
    }
    Ok(())
}

fn power_refs(record: &DeviceRecord, by_code: &BTreeMap<String, DpRef>) -> Vec<DpRef> {
    // Одиночный выключатель, затем многоканальный. Не смешиваем `switch_led`
    // с реле, если облако отдало оба: это разные цепи.
    for code in ["switch_led", "switch", "power", "switch_1"] {
        if let Some(dp) = by_code.get(code) {
            let mut out = vec![dp.clone()];
            if code == "switch_led" || code == "switch" {
                return out;
            }
            // switch_1 найден: добавляем остальные каналы.
            let mut extra: Vec<DpRef> = by_code
                .iter()
                .filter(|(name, _)| {
                    name.starts_with("switch_")
                        && name[7..].chars().all(|c| c.is_ascii_digit())
                        && name.as_str() != code
                })
                .map(|(_, dp)| dp.clone())
                .collect();
            extra.sort_by(|a, b| a.code.cmp(&b.code));
            out.extend(extra);
            return out;
        }
    }
    let _ = record;
    Vec::new()
}

/// Семантическая команда, которую строит инструмент.
#[derive(Clone, Debug, Default)]
pub struct ChangeSet {
    pub power: Option<bool>,
    pub brightness: Option<u8>,
    pub color: Option<String>,
    pub color_temp: Option<u8>,
    pub mode: Option<String>,
    pub temperature: Option<f32>,
    pub fan_speed: Option<String>,
    pub position: Option<u8>,
}

impl ChangeSet {
    pub fn is_empty(&self) -> bool {
        self.power.is_none()
            && self.brightness.is_none()
            && self.color.is_none()
            && self.color_temp.is_none()
            && self.mode.is_none()
            && self.temperature.is_none()
            && self.fan_speed.is_none()
            && self.position.is_none()
    }
}

/// План — только `dpId -> значение`, без кодов и произвольных параметров.
pub fn plan(
    record: &DeviceRecord,
    caps: &Capabilities,
    req: &ChangeSet,
) -> Result<Vec<(String, Value)>, String> {
    if !record.is_usable_local() {
        return Err(if record.unsupported_reason.is_empty() {
            "локальное управление недоступно".into()
        } else {
            record.unsupported_reason.clone()
        });
    }
    if req.is_empty() {
        return Err("не указано, что изменить".into());
    }
    let mut out: Vec<(String, Value)> = Vec::new();

    if let Some(power) = req.power {
        if caps.power.is_empty() {
            return Err("устройство не поддерживает включение".into());
        }
        for dp in &caps.power {
            out.push((dp.dp_id.clone(), Value::Bool(power)));
        }
        if !power {
            // Выключение не сопровождается яркостью/цветом: устройство может
            // применить их после включения неожиданно.
            return Ok(out);
        }
    }

    let mut wants_on = false;
    if let Some(percent) = req.brightness {
        let dp = caps
            .brightness
            .as_ref()
            .ok_or_else(|| "устройство не поддерживает яркость".to_string())?;
        out.push((dp.dp_id.clone(), json!(raw_percent(dp, percent)?)));
        wants_on = true;
    }
    if let Some(percent) = req.color_temp {
        let dp = caps
            .color_temp
            .as_ref()
            .ok_or_else(|| "устройство не поддерживает температуру света".to_string())?;
        out.push((dp.dp_id.clone(), json!(raw_percent(dp, percent)?)));
        wants_on = true;
        if let Some(mode) = &caps.mode {
            if mode
                .format
                .values
                .iter()
                .any(|v| v.eq_ignore_ascii_case("white"))
            {
                out.push((mode.dp_id.clone(), json!("white")));
            }
        }
    }
    if let Some(color) = &req.color {
        let dp = caps
            .color
            .as_ref()
            .ok_or_else(|| "устройство не поддерживает цвет".to_string())?;
        let rgb = parse_color(color)?;
        let value = if dp.format.kind.eq_ignore_ascii_case("json") {
            Value::String(color_json(rgb))
        } else {
            Value::String(format!("#{:02X}{:02X}{:02X}", rgb.0, rgb.1, rgb.2))
        };
        out.push((dp.dp_id.clone(), value));
        wants_on = true;
        if let Some(mode) = &caps.mode {
            let enabled = mode
                .format
                .values
                .iter()
                .find(|v| v.eq_ignore_ascii_case("colour") || v.eq_ignore_ascii_case("color"));
            if let Some(value) = enabled {
                out.push((mode.dp_id.clone(), Value::String(value.clone())));
            }
        }
    }
    if let Some(mode) = &req.mode {
        let dp = caps
            .mode
            .as_ref()
            .ok_or_else(|| "устройство не поддерживает режимы".to_string())?;
        let value = match_enum(dp, mode)?;
        out.push((dp.dp_id.clone(), Value::String(value)));
        wants_on = true;
    }
    if let Some(temp) = req.temperature {
        let dp = caps
            .temp_set
            .as_ref()
            .ok_or_else(|| "устройство не поддерживает целевую температуру".to_string())?;
        out.push((dp.dp_id.clone(), json!(raw_temperature(dp, temp)?)));
        wants_on = true;
    }
    if let Some(fan) = &req.fan_speed {
        let dp = caps
            .fan
            .as_ref()
            .ok_or_else(|| "устройство не поддерживает скорость вентилятора".to_string())?;
        out.push((dp.dp_id.clone(), Value::String(match_enum(dp, fan)?)));
        wants_on = true;
    }
    if let Some(position) = req.position {
        let dp = caps
            .position
            .as_ref()
            .ok_or_else(|| "устройство не поддерживает положение".to_string())?;
        let raw = raw_percent(dp, position)?;
        out.push((dp.dp_id.clone(), json!(raw)));
    }

    if wants_on && req.power.is_none() && !caps.power.is_empty() {
        for dp in &caps.power {
            if !out.iter().any(|(id, _)| id == &dp.dp_id) {
                out.push((dp.dp_id.clone(), Value::Bool(true)));
            }
        }
    }
    Ok(out)
}

fn match_enum(dp: &DpRef, requested: &str) -> Result<String, String> {
    dp.format
        .values
        .iter()
        .find(|v| v.eq_ignore_ascii_case(requested.trim()))
        .cloned()
        .ok_or_else(|| {
            format!(
                "режим «{requested}» не поддерживается; допустимо: {}",
                dp.format.values.join(", ")
            )
        })
}

pub fn raw_percent(dp: &DpRef, percent: u8) -> Result<i64, String> {
    let (min, max) = range(dp)?;
    let p = f64::from(percent).clamp(0.0, 100.0);
    let raw = (min + (max - min) * p / 100.0).round();
    Ok(clamp_raw(dp, raw))
}

pub fn percent_from_raw(dp: &DpRef, raw: f64) -> Option<u8> {
    let (min, max) = range(dp).ok()?;
    if (max - min).abs() < f64::EPSILON {
        return None;
    }
    Some((((raw - min) * 100.0 / (max - min)).round()).clamp(0.0, 100.0) as u8)
}

pub fn raw_temperature(dp: &DpRef, celsius: f32) -> Result<i64, String> {
    range(dp)?;
    let mut value = f64::from(celsius);
    if dp.code.ends_with("_f") {
        value = value * 9.0 / 5.0 + 32.0;
    }
    let scale = dp.format.scale.unwrap_or(0).clamp(0, 3);
    let raw = (value * 10f64.powi(scale)).round();
    Ok(clamp_raw(dp, raw))
}

pub fn temperature_from_raw(dp: &DpRef, raw: f64) -> Option<f32> {
    let scale = dp.format.scale.unwrap_or(0).clamp(0, 3);
    let mut value = raw / 10f64.powi(scale);
    if dp.code.ends_with("_f") {
        value = (value - 32.0) * 5.0 / 9.0;
    }
    Some(value as f32)
}

fn range(dp: &DpRef) -> Result<(f64, f64), String> {
    let min = dp
        .format
        .min
        .ok_or_else(|| format!("у {} нет минимума", dp.code))?;
    let max = dp
        .format
        .max
        .ok_or_else(|| format!("у {} нет максимума", dp.code))?;
    if min >= max {
        return Err(format!("у {} пустой диапазон", dp.code));
    }
    Ok((min, max))
}

fn clamp_raw(dp: &DpRef, raw: f64) -> i64 {
    let min = dp.format.min.unwrap_or(raw);
    let max = dp.format.max.unwrap_or(raw);
    raw.clamp(min, max).round() as i64
}

/// `#RRGGBB` -> RGB.
pub fn parse_color(value: &str) -> Result<(u8, u8, u8), String> {
    let hex = value.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("цвет должен быть в формате #RRGGBB".into());
    }
    let r = u8::from_str_radix(&hex[0..2], 16).map_err(|_| "неверный цвет")?;
    let g = u8::from_str_radix(&hex[2..4], 16).map_err(|_| "неверный цвет")?;
    let b = u8::from_str_radix(&hex[4..6], 16).map_err(|_| "неверный цвет")?;
    Ok((r, g, b))
}

fn value_to_bool(value: &Value) -> Option<bool> {
    value
        .as_bool()
        .or_else(|| value.as_i64().map(|v| v != 0))
        .or_else(|| {
            value.as_str().map(|s| {
                matches!(
                    s.trim().to_ascii_lowercase().as_str(),
                    "true" | "1" | "pir" | "motion" | "open" | "opened"
                )
            })
        })
}

fn color_json(rgb: (u8, u8, u8)) -> String {
    let (r, g, b) = rgb;
    let rf = f64::from(r) / 255.0;
    let gf = f64::from(g) / 255.0;
    let bf = f64::from(b) / 255.0;
    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    let delta = max - min;
    let hue = if delta.abs() < f64::EPSILON {
        0.0
    } else if (max - rf).abs() < f64::EPSILON {
        60.0 * (((gf - bf) / delta) % 6.0)
    } else if (max - gf).abs() < f64::EPSILON {
        60.0 * ((bf - rf) / delta + 2.0)
    } else {
        60.0 * ((rf - gf) / delta + 4.0)
    };
    let hue = if hue < 0.0 { hue + 360.0 } else { hue };
    let sat = if max.abs() < f64::EPSILON {
        0.0
    } else {
        delta / max
    };
    let value = max;
    json!({
        "h": hue.round().clamp(0.0, 360.0) as i64,
        "s": (sat * 1000.0).round().clamp(0.0, 1000.0) as i64,
        "v": (value * 1000.0).round().clamp(0.0, 1000.0) as i64
    })
    .to_string()
}

/// Разобранное состояние для голосового ответа и проверки выполнения.
#[derive(Clone, Debug, Default)]
pub struct DeviceState {
    pub power: Option<bool>,
    pub power_gangs: Vec<bool>,
    pub brightness: Option<u8>,
    pub color_temp: Option<u8>,
    pub color: Option<String>,
    pub mode: Option<String>,
    pub temperature: Option<f32>,
    pub target_temperature: Option<f32>,
    pub humidity: Option<f32>,
    pub motion: Option<bool>,
    pub contact: Option<bool>,
    pub fan: Option<String>,
    pub position: Option<u8>,
}

pub fn decode_state(record: &DeviceRecord, dps: &BTreeMap<String, Value>) -> DeviceState {
    let caps = capabilities(record).ok();
    let mut state = DeviceState::default();
    for (dp_id, value) in dps {
        let Some(info) = record.dp_map.get(dp_id) else {
            continue;
        };
        let dp = DpRef {
            dp_id: dp_id.clone(),
            code: info.code.clone(),
            format: effective_format(record, info),
        };
        let code = info.code.as_str();
        if caps
            .as_ref()
            .is_some_and(|c| c.power.iter().any(|p| p.dp_id == dp.dp_id))
        {
            let on = value.as_bool().or_else(|| value.as_i64().map(|v| v != 0));
            if let Some(on) = on {
                state.power_gangs.push(on);
            }
            continue;
        }
        if caps
            .as_ref()
            .and_then(|c| c.brightness.as_ref())
            .is_some_and(|p| p.dp_id == dp.dp_id)
        {
            if let Some(raw) = value.as_f64() {
                state.brightness = percent_from_raw(&dp, raw);
            }
            continue;
        }
        if caps
            .as_ref()
            .and_then(|c| c.color_temp.as_ref())
            .is_some_and(|p| p.dp_id == dp.dp_id)
        {
            if let Some(raw) = value.as_f64() {
                state.color_temp = percent_from_raw(&dp, raw);
            }
            continue;
        }
        if caps
            .as_ref()
            .and_then(|c| c.color.as_ref())
            .is_some_and(|p| p.dp_id == dp.dp_id)
        {
            state.color = value.as_str().map(str::to_owned);
            continue;
        }
        if caps
            .as_ref()
            .and_then(|c| c.mode.as_ref())
            .is_some_and(|p| p.dp_id == dp.dp_id)
        {
            state.mode = value_to_text(value);
            continue;
        }
        if caps
            .as_ref()
            .and_then(|c| c.temp_set.as_ref())
            .is_some_and(|p| p.dp_id == dp.dp_id)
        {
            if let Some(raw) = value.as_f64() {
                state.target_temperature = temperature_from_raw(&dp, raw);
            }
            continue;
        }
        if caps
            .as_ref()
            .and_then(|c| c.temp_current.as_ref())
            .is_some_and(|p| p.dp_id == dp.dp_id)
            || matches!(code, "temp_current" | "temp_current_f" | "va_temperature")
        {
            if let Some(raw) = value.as_f64() {
                state.temperature = temperature_from_raw(&dp, raw);
            }
            continue;
        }
        if matches!(code, "humidity_value" | "va_humidity" | "humidity") {
            if let Some(raw) = value.as_f64() {
                let scale = dp.format.scale.unwrap_or(0).clamp(0, 3);
                state.humidity = Some((raw / 10f64.powi(scale)) as f32);
            }
            continue;
        }
        if matches!(code, "pir" | "motion" | "motion_switch") {
            state.motion = value_to_bool(value);
            continue;
        }
        if matches!(
            code,
            "doorcontact_state" | "contact_state" | "door_state" | "contact"
        ) {
            state.contact = value_to_bool(value);
            continue;
        }
        if caps
            .as_ref()
            .and_then(|c| c.fan.as_ref())
            .is_some_and(|p| p.dp_id == dp.dp_id)
        {
            state.fan = value_to_text(value);
            continue;
        }
        if caps
            .as_ref()
            .and_then(|c| c.position.as_ref())
            .is_some_and(|p| p.dp_id == dp.dp_id)
            || code == "percent_state"
        {
            if let Some(raw) = value.as_f64() {
                state.position = percent_from_raw(&dp, raw);
            }
        }
    }
    if !state.power_gangs.is_empty() {
        state.power = Some(state.power_gangs.iter().any(|on| *on));
    }
    state
}

fn value_to_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(if *b { "on".into() } else { "off".into() }),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Человеческое описание состояния. Без ключей и идентификаторов.
pub fn describe_state(record: &DeviceRecord, state: &DeviceState) -> String {
    let mut parts = Vec::new();
    if let Some(on) = state.power {
        if state.power_gangs.len() > 1 {
            let count = state.power_gangs.iter().filter(|on| **on).count();
            parts.push(format!(
                "каналов включено {count} из {}",
                state.power_gangs.len()
            ));
        } else {
            parts.push(if on {
                "включено".to_string()
            } else {
                "выключено".to_string()
            });
        }
    }
    if let Some(b) = state.brightness {
        parts.push(format!("яркость {b}%"));
    }
    if let Some(t) = state.color_temp {
        parts.push(format!("температура света {t}%"));
    }
    if let Some(c) = &state.color {
        parts.push(format!("цвет {c}"));
    }
    if let Some(m) = &state.mode {
        parts.push(format!("режим {m}"));
    }
    if let Some(t) = state.temperature {
        parts.push(format!("в комнате {:.1} °C", t));
    }
    if let Some(t) = state.target_temperature {
        parts.push(format!("цель {:.1} °C", t));
    }
    if let Some(h) = state.humidity {
        parts.push(format!("влажность {h:.0}%"));
    }
    if let Some(motion) = state.motion {
        parts.push(
            if motion {
                "движение есть"
            } else {
                "движения нет"
            }
            .into(),
        );
    }
    if let Some(contact) = state.contact {
        parts.push(
            if contact {
                "открыто"
            } else {
                "закрыто"
            }
            .into(),
        );
    }
    if let Some(f) = &state.fan {
        parts.push(format!("вентилятор {f}"));
    }
    if let Some(p) = state.position {
        parts.push(format!("положение {p}%"));
    }
    if parts.is_empty() {
        format!(
            "{}: состояние недоступно (DP не расшифрованы)",
            display_name(record)
        )
    } else {
        format!("{}: {}", display_name(record), parts.join(", "))
    }
}

pub fn display_name(record: &DeviceRecord) -> String {
    match &record.room {
        Some(room) if !room.name.trim().is_empty() => {
            format!("{} ({})", record.name, room.name)
        }
        _ => record.name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smart_home::model::{DpFormat, DpInfo, FunctionSpec, Room, StatusSpec};
    use std::collections::BTreeMap;

    fn dp(
        dp_id: &str,
        code: &str,
        kind: &str,
        min: Option<f64>,
        max: Option<f64>,
        values: &[&str],
    ) -> (String, DpInfo) {
        (
            dp_id.into(),
            DpInfo {
                dp_id: dp_id.into(),
                code: code.into(),
                format: DpFormat {
                    kind: kind.into(),
                    scale: Some(0),
                    min,
                    max,
                    step: None,
                    unit: String::new(),
                    values: values.iter().map(|v| v.to_string()).collect(),
                },
            },
        )
    }

    fn record(category: &str, dps: Vec<(String, DpInfo)>) -> DeviceRecord {
        let map: BTreeMap<_, _> = dps.into_iter().collect();
        let functions = map
            .values()
            .map(|i| FunctionSpec {
                code: i.code.clone(),
                kind: i.format.kind.clone(),
                values: Value::Null,
            })
            .collect();
        let status = map
            .values()
            .map(|i| StatusSpec {
                code: i.code.clone(),
                kind: i.format.kind.clone(),
                values: Value::Null,
            })
            .collect();
        DeviceRecord {
            id: "dev-1".into(),
            name: "Люстра".into(),
            category: category.into(),
            product_id: "p".into(),
            product_name: "Свет".into(),
            model: String::new(),
            local_key: "0123456789abcdef".into(),
            sub: false,
            gateway_id: None,
            node_id: None,
            online: true,
            room: Some(Room {
                id: "1".into(),
                name: "Спальня".into(),
            }),
            home_id: "h".into(),
            home_name: "Дом".into(),
            functions,
            status,
            dp_map: map,
            cloud_status: BTreeMap::new(),
            local_supported: true,
            unsupported_reason: String::new(),
            ip: Some("192.168.1.10".into()),
            version: None,
            device22: false,
            last_seen: 0,
            key_from: None,
        }
    }

    #[test]
    fn policy_allows_only_safe_categories() {
        assert_eq!(policy_for("dj"), CategoryPolicy::Control(DeviceKind::Light));
        assert_eq!(policy_for("cz"), CategoryPolicy::Control(DeviceKind::Plug));
        assert_eq!(
            policy_for("kt"),
            CategoryPolicy::Control(DeviceKind::Climate)
        );
        assert_eq!(
            policy_for("wsdcg"),
            CategoryPolicy::ReadOnly(DeviceKind::Sensor)
        );
        assert_eq!(policy_for("ms"), CategoryPolicy::Forbidden);
        assert_eq!(policy_for("ipc"), CategoryPolicy::Forbidden);
        assert_eq!(
            policy_for("mcs"),
            CategoryPolicy::ReadOnly(DeviceKind::Sensor)
        );
        assert!(is_critical_category("ms"));
        assert!(is_critical_category("ipc"));
        assert!(!is_critical_category("mcs"));
        assert!(!is_critical_category("dj"));
    }

    #[test]
    fn builds_brightness_command_with_scale_and_power() {
        let r = record(
            "dj",
            vec![
                dp("1", "switch_led", "Bool", None, None, &[]),
                dp(
                    "2",
                    "bright_value",
                    "Integer",
                    Some(10.0),
                    Some(1000.0),
                    &[],
                ),
            ],
        );
        let caps = capabilities(&r).unwrap();
        let plan = plan(
            &r,
            &caps,
            &ChangeSet {
                brightness: Some(40),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(plan.contains(&("2".into(), json!(406))));
        assert!(plan.contains(&("1".into(), Value::Bool(true))));
    }

    #[test]
    fn unknown_dp_is_never_sent() {
        let r = record("dj", vec![dp("1", "switch_led", "Bool", None, None, &[])]);
        let caps = capabilities(&r).unwrap();
        let err = plan(
            &r,
            &caps,
            &ChangeSet {
                temperature: Some(23.0),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.contains("температуру"));
    }

    #[test]
    fn colour_temp_is_rejected_on_plain_plug() {
        let r = record("cz", vec![dp("1", "switch_1", "Bool", None, None, &[])]);
        let caps = capabilities(&r).unwrap();
        assert!(caps.color_temp.is_none());
        assert!(plan(
            &r,
            &caps,
            &ChangeSet {
                color_temp: Some(50),
                ..Default::default()
            }
        )
        .is_err());
    }

    #[test]
    fn enum_mode_must_be_known() {
        let r = record(
            "kt",
            vec![
                dp("1", "switch_1", "Bool", None, None, &[]),
                dp("2", "mode", "Enum", None, None, &["cold", "hot", "wind"]),
            ],
        );
        let caps = capabilities(&r).unwrap();
        let ok = plan(
            &r,
            &caps,
            &ChangeSet {
                mode: Some("COLD".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(ok.contains(&("2".into(), json!("cold"))));
        let err = plan(
            &r,
            &caps,
            &ChangeSet {
                mode: Some("turbo".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.contains("допустимо"));
    }

    #[test]
    fn state_decoding_uses_dp_map() {
        let r = record(
            "dj",
            vec![
                dp("1", "switch_led", "Bool", None, None, &[]),
                dp(
                    "2",
                    "bright_value",
                    "Integer",
                    Some(10.0),
                    Some(1000.0),
                    &[],
                ),
                dp("3", "temp_value", "Integer", Some(0.0), Some(1000.0), &[]),
            ],
        );
        let mut dps = BTreeMap::new();
        dps.insert("1".to_string(), json!(true));
        dps.insert("2".to_string(), json!(406));
        dps.insert("3".to_string(), json!(750));
        let state = decode_state(&r, &dps);
        assert_eq!(state.power, Some(true));
        assert_eq!(state.brightness, Some(40));
        assert_eq!(state.color_temp, Some(75));
    }

    #[test]
    fn temperature_fahrenheit_roundtrip() {
        let mut spec = dp("1", "temp_set_f", "Integer", Some(320.0), Some(1040.0), &[]);
        spec.1.format.scale = Some(1);
        let r = record("kt", vec![spec]);
        let caps = capabilities(&r).unwrap();
        let dp = caps.temp_set.as_ref().unwrap();
        let raw = raw_temperature(dp, 23.0).unwrap();
        let back = temperature_from_raw(dp, raw as f64).unwrap();
        assert!((back - 23.0).abs() < 0.15, "{back}");
    }
}
