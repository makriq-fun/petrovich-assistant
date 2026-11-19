//! Погода через Open-Meteo: без API-ключа, координаты из конфигурации.
//! Endpoint заменяем, ответ проверяется по схеме.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;
use url::Url;

#[derive(Clone)]
pub struct Weather {
    client: reqwest::Client,
    url: String,
    latitude: f64,
    longitude: f64,
    place: String,
    allow_private: bool,
    max_bytes: usize,
}

impl Weather {
    pub fn new(
        url: impl Into<String>,
        latitude: f64,
        longitude: f64,
        place: impl Into<String>,
        timeout_ms: u64,
        allow_private: bool,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("petrovich-assistant/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("HTTP-клиент погоды")?;
        Ok(Self {
            client,
            url: url.into(),
            latitude,
            longitude,
            place: place.into(),
            allow_private,
            max_bytes: 256 * 1024,
        })
    }

    pub async fn current(&self) -> Result<String> {
        let mut url = Url::parse(&self.url).context("weather_url")?;
        if !self.allow_private && url.scheme() != "https" {
            bail!("погода: нужен HTTPS");
        }
        url.query_pairs_mut()
            .append_pair("latitude", &format!("{:.4}", self.latitude))
            .append_pair("longitude", &format!("{:.4}", self.longitude))
            .append_pair(
                "current",
                "temperature_2m,apparent_temperature,relative_humidity_2m,precipitation,weather_code,wind_speed_10m",
            )
            .append_pair(
                "daily",
                "temperature_2m_max,temperature_2m_min,precipitation_probability_max,weather_code",
            )
            .append_pair("timezone", "auto")
            .append_pair("forecast_days", "1");
        let resp = self
            .client
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("запрос погоды {url}"))?;
        if !resp.status().is_success() {
            bail!("погода: HTTP {}", resp.status());
        }
        let bytes = crate::http_read(resp, self.max_bytes).await?;
        let v: Value = serde_json::from_slice(&bytes).context("погода: не JSON")?;
        self.format(&v)
    }

    fn format(&self, v: &Value) -> Result<String> {
        let cur = v.get("current").context("погода: нет current")?;
        let num = |obj: &Value, key: &str| obj.get(key).and_then(Value::as_f64);
        let temp = num(cur, "temperature_2m").context("погода: нет температуры")?;
        let feels = num(cur, "apparent_temperature");
        let humidity = num(cur, "relative_humidity_2m");
        let wind = num(cur, "wind_speed_10m");
        let precip = num(cur, "precipitation").unwrap_or(0.0);
        let code = cur
            .get("weather_code")
            .and_then(Value::as_i64)
            .unwrap_or(-1);
        let daily = v.get("daily");
        let day = |key: &str| {
            daily
                .and_then(|d| d.get(key))
                .and_then(|a| a.get(0))
                .and_then(Value::as_f64)
        };
        let tmax = day("temperature_2m_max");
        let tmin = day("temperature_2m_min");
        let rain = day("precipitation_probability_max");
        let mut out = String::new();
        if !self.place.is_empty() {
            out.push_str(&self.place);
            out.push_str(": ");
        }
        out.push_str(&format!("{} {:.0} °C", description(code), temp));
        if let Some(f) = feels {
            if (f - temp).abs() >= 1.0 {
                out.push_str(&format!(", ощущается как {f:.0}"));
            }
        }
        if let Some(h) = humidity {
            out.push_str(&format!(", влажность {h:.0}%"));
        }
        if let Some(w) = wind {
            out.push_str(&format!(", ветер {w:.1} м/с"));
        }
        if precip > 0.0 {
            out.push_str(&format!(", осадки {precip:.1} мм"));
        }
        if let (Some(max), Some(min)) = (tmax, tmin) {
            out.push_str(&format!("; днём от {min:.0} до {max:.0} °C"));
        }
        if let Some(r) = rain {
            if r >= 20.0 {
                out.push_str(&format!(", вероятность дождя {r:.0}%"));
            }
        }
        out.push('.');
        Ok(out)
    }
}

/// Русское описание кода WMO.
pub fn description(code: i64) -> &'static str {
    match code {
        0 => "ясно",
        1 => "в основном ясно",
        2 => "переменная облачность",
        3 => "пасмурно",
        45 | 48 => "туман",
        51 | 53 | 55 => "морось",
        56 | 57 => "ледяная морось",
        61 => "небольшой дождь",
        63 => "дождь",
        65 => "сильный дождь",
        66 | 67 => "ледяной дождь",
        71 => "небольшой снег",
        73 => "снег",
        75 => "сильный снег",
        77 => "снежные зёрна",
        80 | 81 => "ливень",
        82 => "сильный ливень",
        85 | 86 => "снегопад",
        95 => "гроза",
        96 | 99 => "гроза с градом",
        _ => "погода без описания",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::serve_once;

    fn body() -> String {
        r#"{
          "current": {"temperature_2m": 12.4, "apparent_temperature": 10.1,
            "relative_humidity_2m": 70, "precipitation": 0.0,
            "weather_code": 2, "wind_speed_10m": 3.2},
          "daily": {"temperature_2m_max": [15.2], "temperature_2m_min": [7.1],
            "precipitation_probability_max": [10], "weather_code": [2]}
        }"#
        .into()
    }

    #[tokio::test]
    async fn formats_current_weather() {
        let (url, handle) = serve_once(200, "application/json", &body()).await;
        let w = Weather::new(url, 55.75, 37.61, "Москва", 5000, true).unwrap();
        let text = w.current().await.unwrap();
        assert!(text.contains("Москва"), "{text}");
        assert!(text.contains("12"), "{text}");
        assert!(text.contains("переменная облачность"), "{text}");
        assert!(text.contains("ветер"), "{text}");
        handle.await.unwrap();
    }

    #[test]
    fn wmo_codes_are_russian() {
        assert_eq!(description(0), "ясно");
        assert_eq!(description(95), "гроза");
        assert_eq!(description(999), "погода без описания");
    }
}
