//! Чтение веб-страниц и поиск через Tavily. HTTPS обязателен, частные
//! адреса закрыты, редиректы отключены: адрес проверяется один раз и не
//! подменяется.

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use url::Url;

use crate::html;

#[derive(Clone)]
pub struct Web {
    client: reqwest::Client,
    search_client: reqwest::Client,
    search_api_key: String,
    allow_private: bool,
    max_bytes: usize,
    max_chars: usize,
}

#[derive(Debug)]
pub struct Page {
    pub url: String,
    pub title: Option<String>,
    pub text: String,
}

impl Web {
    pub fn new(timeout_ms: u64, max_bytes: usize, allow_private: bool) -> Result<Self> {
        Self::with_search_proxy(timeout_ms, max_bytes, allow_private, "")
    }

    /// Клиент чтения через SOCKS/HTTP-прокси — для проверки обхода.
    pub fn with_fetch_proxy(
        timeout_ms: u64,
        max_bytes: usize,
        allow_private: bool,
        proxy: &str,
    ) -> Result<Self> {
        let mut web = Self::with_search_proxy(timeout_ms, max_bytes, allow_private, "")?;
        if !proxy.trim().is_empty() {
            web.client = reqwest::Client::builder()
                .timeout(Duration::from_millis(timeout_ms))
                .connect_timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("petrovich-assistant/", env!("CARGO_PKG_VERSION")))
                .proxy(reqwest::Proxy::all(proxy.trim())?)
                .build()
                .context("HTTP-клиент с прокси")?;
        }
        Ok(web)
    }

    /// `search_proxy` применяется только к Tavily: чтение страниц идёт
    /// напрямую, чтобы прокси не влиял на проверку адресов.
    pub fn with_search_proxy(
        timeout_ms: u64,
        max_bytes: usize,
        allow_private: bool,
        search_proxy: &str,
    ) -> Result<Self> {
        let build = |proxy: Option<&str>| -> Result<reqwest::Client> {
            let mut b = reqwest::Client::builder()
                .timeout(Duration::from_millis(timeout_ms))
                .connect_timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("petrovich-assistant/", env!("CARGO_PKG_VERSION")));
            if let Some(proxy) = proxy {
                b = b.proxy(
                    reqwest::Proxy::all(proxy).with_context(|| format!("search_proxy {proxy}"))?,
                );
            }
            b.build().context("HTTP-клиент web")
        };
        let client = build(None)?;
        let search_client = if search_proxy.trim().is_empty() {
            build(None)?
        } else {
            build(Some(search_proxy.trim()))?
        };
        Ok(Self {
            client,
            search_client,
            search_api_key: String::new(),
            allow_private,
            max_bytes,
            max_chars: 6000,
        })
    }

    pub async fn fetch(&self, raw: &str) -> Result<Page> {
        let url = self.validate(raw).await?;
        let resp = self
            .client
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("запрос {url}"))?;
        if !resp.status().is_success() {
            bail!("web: HTTP {} для {url}", resp.status());
        }
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !(ctype.starts_with("text/")
            || ctype.contains("json")
            || ctype.contains("xml")
            || ctype.is_empty())
        {
            bail!("web: неподдерживаемый тип {ctype}");
        }
        let bytes = crate::http_read(resp, self.max_bytes).await?;
        let body = String::from_utf8_lossy(&bytes);
        if ctype.contains("json") {
            return Ok(Page {
                url: url.to_string(),
                title: None,
                text: body.chars().take(self.max_chars).collect(),
            });
        }
        let (title, text) = html::extract(&body, self.max_chars);
        Ok(Page {
            url: url.to_string(),
            title,
            text,
        })
    }

    pub fn with_search_api(mut self, key: impl Into<String>) -> Self {
        self.search_api_key = key.into();
        self
    }

    pub fn has_search_api(&self) -> bool {
        !self.search_api_key.is_empty()
    }

    /// Поиск только через Tavily. Устаревшие парсеры SearXNG и DuckDuckGo
    /// удалены: они ломались от изменений разметки и не давали стабильных
    /// результатов на колонке.
    pub async fn search(&self, query: &str) -> Result<Vec<SearchHit>> {
        if !self.has_search_api() {
            bail!("веб-поиск не настроен: задайте Tavily API-ключ");
        }
        self.search_tavily(query).await
    }

    async fn search_tavily(&self, query: &str) -> Result<Vec<SearchHit>> {
        let body = serde_json::json!({
            "query": query,
            "search_depth": "basic",
            "max_results": 5,
            "include_answer": false,
            "include_raw_content": false,
        });
        let resp = self
            .search_client
            .post("https://api.tavily.com/search")
            .bearer_auth(&self.search_api_key)
            .json(&body)
            .send()
            .await
            .context("Tavily: запрос")?;
        if !resp.status().is_success() {
            bail!("Tavily: HTTP {}", resp.status());
        }
        let bytes = crate::http_read(resp, self.max_bytes).await?;
        parse_tavily(&bytes)
    }

    async fn validate(&self, raw: &str) -> Result<Url> {
        let url = Url::parse(raw.trim()).context("web: неверный URL")?;
        if !url.username().is_empty() || url.password().is_some() {
            bail!("web: URL с учётными данными запрещён");
        }
        if !self.allow_private {
            check_scheme(&url)?;
        }
        let host = url.host_str().context("web: нет хоста")?.to_owned();
        if !self.allow_private {
            if is_private_name(&host) {
                bail!("web: частный адрес запрещён");
            }
            let port = url.port_or_known_default().unwrap_or(443);
            let addrs: Vec<IpAddr> = tokio::net::lookup_host((host.as_str(), port))
                .await
                .with_context(|| format!("web: DNS {host}"))?
                .map(|a| a.ip())
                .collect();
            if addrs.is_empty() {
                bail!("web: {host} не разрешается");
            }
            if addrs.iter().any(|ip| is_private_ip(*ip)) {
                bail!("web: {host} указывает на частный адрес");
            }
        }
        Ok(url)
    }
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

fn parse_tavily(bytes: &[u8]) -> Result<Vec<SearchHit>> {
    let v: serde_json::Value = serde_json::from_slice(bytes).context("Tavily: не JSON")?;
    let arr = v
        .get("results")
        .and_then(|r| r.as_array())
        .context("Tavily: нет results")?;
    Ok(arr
        .iter()
        .filter_map(|r| {
            Some(SearchHit {
                title: r.get("title")?.as_str()?.to_owned(),
                url: r.get("url")?.as_str()?.to_owned(),
                snippet: r
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .chars()
                    .take(300)
                    .collect(),
            })
        })
        .take(5)
        .collect())
}

fn check_scheme(url: &Url) -> Result<()> {
    if url.scheme() != "https" {
        bail!("web: разрешён только HTTPS (получено {})", url.scheme());
    }
    Ok(())
}

fn is_private_name(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    h == "localhost"
        || h.ends_with(".localhost")
        || h.ends_with(".local")
        || h.ends_with(".internal")
}

pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_documentation()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::serve_once;

    #[tokio::test]
    async fn fetches_and_extracts_page() {
        let (url, handle) = serve_once(
            200,
            "text/html",
            "<html><head><title>Т</title></head><body><p>Привет <b>мир</b></p></body></html>",
        )
        .await;
        let w = Web::new(5000, 100_000, true).unwrap();
        let page = w.fetch(&url).await.unwrap();
        assert_eq!(page.title.as_deref(), Some("Т"));
        assert_eq!(page.text, "Привет мир");
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_private_and_http_by_default() {
        let w = Web::new(5000, 100_000, false).unwrap();
        assert!(w.validate("http://example.com/").await.is_err());
        assert!(w.validate("https://127.0.0.1/").await.is_err());
        assert!(w.validate("https://localhost/").await.is_err());
        assert!(w.validate("https://user:pass@example.com/").await.is_err());
    }

    #[test]
    fn parses_tavily_results() {
        let body = r#"{"results":[{"title":"A","url":"https://a","content":"one"},{"title":"B","url":"https://b","content":"two"}]}"#;
        let hits = parse_tavily(body.as_bytes()).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "A");
        assert_eq!(hits[0].url, "https://a");
        assert_eq!(hits[0].snippet, "one");
    }

    #[tokio::test]
    async fn search_requires_tavily_key() {
        let w = Web::new(5000, 100_000, true).unwrap();
        assert!(w.search("погода").await.is_err());
    }

    #[test]
    fn private_ip_classes() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.1",
            "169.254.1.1",
            "::1",
            "fd00::1",
        ] {
            assert!(is_private_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "2606:4700::1111"] {
            assert!(!is_private_ip(ip.parse().unwrap()), "{ip}");
        }
    }
}
