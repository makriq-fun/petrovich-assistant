//! Сахар из Nightscout: текущее значение, тренд и история за несколько часов.
//! Используется только чтение API v1; endpoint и токен задаются конфигурацией.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;
use url::Url;

const MGDL_PER_MMOL: f64 = 18.0;
const DEFAULT_HOURS: u64 = 3;
const MAX_HOURS: u64 = 24;
/// Окно, по которому считается изменение: CGM пишет примерно раз в 5 минут.
const DELTA_MINUTES: i64 = 15;

/// Показатели для экрана и голосового ответа.
#[derive(Clone, Debug)]
pub struct SugarNow {
    pub text: String,
    pub mmol: f32,
    pub delta: f32,
    pub trend: String,
}

#[derive(Clone)]
pub struct Nightscout {
    client: reqwest::Client,
    url: String,
    token: String,
    allow_private: bool,
    max_bytes: usize,
}

struct Entry {
    date: i64,
    sgv: f64,
    direction: String,
}

impl Nightscout {
    pub fn new(
        url: impl Into<String>,
        token: impl Into<String>,
        timeout_ms: u64,
        allow_private: bool,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("petrovich-assistant/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("HTTP-клиент Nightscout")?;
        Ok(Self {
            client,
            url: url.into(),
            token: token.into(),
            allow_private,
            max_bytes: 256 * 1024,
        })
    }

    /// Сколько часов истории показать; без аргумента — три часа.
    pub async fn sugar(&self, hours: Option<u64>) -> Result<String> {
        Ok(self.sugar_data(hours).await?.text)
    }

    pub async fn sugar_data(&self, hours: Option<u64>) -> Result<SugarNow> {
        let hours = hours.unwrap_or(DEFAULT_HOURS).clamp(1, MAX_HOURS);
        let mut url = self.endpoint()?;
        if !self.allow_private && url.scheme() != "https" {
            bail!("Nightscout: нужен HTTPS");
        }
        let count = (hours * 12 + 12).min(1000);
        url.query_pairs_mut()
            .append_pair("count", &count.to_string());
        if !self.token.is_empty() {
            url.query_pairs_mut().append_pair("token", &self.token);
        }
        // URL содержит токен, поэтому не попадает в текст ошибки.
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("запрос Nightscout: {}", e.without_url()))?;
        if !resp.status().is_success() {
            bail!("Nightscout: HTTP {}", resp.status());
        }
        let bytes = crate::http_read(resp, self.max_bytes).await?;
        let v: Value = serde_json::from_slice(&bytes).context("Nightscout: не JSON")?;
        let arr = v
            .as_array()
            .context("Nightscout: ожидается массив записей")?;
        let mut entries = Vec::with_capacity(arr.len());
        for item in arr {
            let (Some(date), Some(sgv)) = (
                item.get("date").and_then(Value::as_i64),
                item.get("sgv").and_then(Value::as_f64),
            ) else {
                continue;
            };
            entries.push(Entry {
                date,
                sgv,
                direction: item
                    .get("direction")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            });
        }
        Ok(format_sugar(&entries, hours))
    }

    fn endpoint(&self) -> Result<Url> {
        let mut url = Url::parse(&self.url).context("nightscout_url")?;
        let path = url.path().trim_end_matches('/');
        if !path.ends_with("/api/v1/entries/sgv.json") {
            url.set_path(&format!("{path}/api/v1/entries/sgv.json"));
        }
        Ok(url)
    }
}

fn format_sugar(entries: &[Entry], hours: u64) -> SugarNow {
    let Some(latest) = entries.iter().max_by_key(|e| e.date) else {
        return SugarNow {
            text: "Нет данных о сахаре в Nightscout.".into(),
            mmol: 0.0,
            delta: 0.0,
            trend: String::new(),
        };
    };
    let now = chrono::Utc::now().timestamp_millis();
    let mmol = latest.sgv / MGDL_PER_MMOL;
    let age_ms = (now - latest.date).max(0);

    let (delta, delta_minutes) = delta(entries, latest);
    let mut out = format!("Сахар {mmol:.1} ммоль/л, {}", glucose_band(mmol));
    let trend = trend_text(&latest.direction, delta);
    if !trend.is_empty() {
        out.push_str(", ");
        out.push_str(trend);
    }
    out.push_str(&format!(". Измерение {}.", age_text(age_ms)));
    if age_ms > 20 * 60_000 {
        out.push_str(" Данные устарели.");
    }
    if let (Some(delta), Some(minutes)) = (delta, delta_minutes) {
        out.push_str(&format!(
            " Изменение за {minutes} мин: {} ммоль/л.",
            signed(delta)
        ));
    }
    if let Some(range) = range(entries, hours, now) {
        out.push_str(&format!(
            " За {hours} ч: от {:.1} до {:.1}, средний {:.1}, измерений {}.",
            range.min, range.max, range.avg, range.count
        ));
    }
    SugarNow {
        text: out,
        mmol: mmol as f32,
        delta: delta.unwrap_or(0.0) as f32,
        trend: trend.to_owned(),
    }
}

fn delta(entries: &[Entry], latest: &Entry) -> (Option<f64>, Option<i64>) {
    let target = latest.date - DELTA_MINUTES * 60_000;
    let Some(reference) = entries
        .iter()
        .filter(|e| e.date < latest.date)
        .min_by_key(|e| (e.date - target).abs())
    else {
        return (None, None);
    };
    let minutes = (latest.date - reference.date) / 60_000;
    // При пропуске дольше десяти минут изменение уже не про 15 минут.
    if !(1..=DELTA_MINUTES + 10).contains(&minutes) {
        return (None, None);
    }
    (
        Some((latest.sgv - reference.sgv) / MGDL_PER_MMOL),
        Some(minutes),
    )
}

struct Range {
    min: f64,
    max: f64,
    avg: f64,
    count: usize,
}

fn range(entries: &[Entry], hours: u64, now: i64) -> Option<Range> {
    let cutoff = now - hours as i64 * 3_600_000;
    let window: Vec<&Entry> = entries.iter().filter(|e| e.date >= cutoff).collect();
    if window.len() < 2 {
        return None;
    }
    let min = window.iter().map(|e| e.sgv).fold(f64::INFINITY, f64::min);
    let max = window
        .iter()
        .map(|e| e.sgv)
        .fold(f64::NEG_INFINITY, f64::max);
    let avg = window.iter().map(|e| e.sgv).sum::<f64>() / window.len() as f64;
    Some(Range {
        min: min / MGDL_PER_MMOL,
        max: max / MGDL_PER_MMOL,
        avg: avg / MGDL_PER_MMOL,
        count: window.len(),
    })
}

fn glucose_band(mmol: f64) -> &'static str {
    if mmol < 3.0 {
        "очень низкий"
    } else if mmol < 3.9 {
        "низкий"
    } else if mmol <= 10.0 {
        "в целевом диапазоне"
    } else if mmol <= 13.9 {
        "высокий"
    } else {
        "очень высокий"
    }
}

fn trend_text(direction: &str, delta: Option<f64>) -> &'static str {
    match direction {
        "TripleUp" => "резко растёт",
        "DoubleUp" => "быстро растёт",
        "SingleUp" => "растёт",
        "FortyFiveUp" => "слегка растёт",
        "Flat" => "стабильный",
        "FortyFiveDown" => "слегка снижается",
        "SingleDown" => "снижается",
        "DoubleDown" => "быстро снижается",
        "TripleDown" => "резко снижается",
        "NOT COMPUTABLE" => "тренд не определён",
        "RATE OUT OF RANGE" => "скачок",
        _ => match delta {
            Some(d) if d >= 0.5 => "быстро растёт",
            Some(d) if d >= 0.15 => "растёт",
            Some(d) if d <= -0.5 => "быстро снижается",
            Some(d) if d <= -0.15 => "снижается",
            Some(_) => "стабильный",
            None => "",
        },
    }
}

fn signed(value: f64) -> String {
    if value.abs() < 0.05 {
        "0.0".into()
    } else {
        format!("{value:+.1}")
    }
}

fn age_text(ms: i64) -> String {
    let minutes = ms / 60_000;
    if minutes < 1 {
        "меньше минуты назад".into()
    } else if minutes < 60 {
        format!("{minutes} мин назад")
    } else {
        let hours = minutes / 60;
        let rest = minutes % 60;
        if rest == 0 {
            format!("{hours} ч назад")
        } else {
            format!("{hours} ч {rest} мин назад")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::serve_once;
    use chrono::Utc;

    fn body(now: i64) -> String {
        format!(
            r#"[
              {{"type":"sgv","sgv":136,"date":{},"direction":"Flat"}},
              {{"type":"sgv","sgv":135,"date":{},"direction":"Flat"}},
              {{"type":"sgv","sgv":151,"date":{},"direction":"SingleDown"}}
            ]"#,
            now - 20 * 60_000,
            now - 5 * 60_000,
            now - 120 * 60_000
        )
    }

    #[tokio::test]
    async fn formats_current_sugar_and_history() {
        let now = Utc::now().timestamp_millis();
        let (url, handle) = serve_once(200, "application/json", &body(now)).await;
        let ns = Nightscout::new(url, "", 5000, true).unwrap();
        let text = ns.sugar(None).await.unwrap();
        assert!(text.contains("7.5 ммоль/л"), "{text}");
        assert!(text.contains("целевом"), "{text}");
        assert!(text.contains("стабильный"), "{text}");
        assert!(text.contains("Изменение за 15 мин"), "{text}");
        assert!(text.contains("За 3 ч: от 7.5 до 8.4"), "{text}");
        handle.await.unwrap();
    }

    #[test]
    fn empty_entries_are_reported_without_error() {
        assert_eq!(
            format_sugar(&[], 3).text,
            "Нет данных о сахаре в Nightscout."
        );
    }

    #[test]
    fn old_reading_is_marked_stale() {
        let now = Utc::now().timestamp_millis();
        let entries = vec![Entry {
            date: now - 40 * 60_000,
            sgv: 108.0,
            direction: "Flat".into(),
        }];
        let text = format_sugar(&entries, 3).text;
        assert!(text.contains("6.0 ммоль/л"), "{text}");
        assert!(text.contains("40 мин назад"), "{text}");
        assert!(text.contains("Данные устарели"), "{text}");
    }

    #[test]
    fn accepts_full_api_path() {
        let ns = Nightscout::new(
            "https://example.test/api/v1/entries/sgv.json",
            "",
            5000,
            true,
        )
        .unwrap();
        assert_eq!(ns.endpoint().unwrap().path(), "/api/v1/entries/sgv.json");
    }

    #[test]
    fn trend_words_are_russian() {
        assert_eq!(trend_text("DoubleDown", None), "быстро снижается");
        assert_eq!(trend_text("", Some(0.4)), "растёт");
        assert_eq!(trend_text("", Some(-0.6)), "быстро снижается");
        assert_eq!(signed(-0.04), "0.0");
        assert_eq!(signed(0.12), "+0.1");
    }
}
