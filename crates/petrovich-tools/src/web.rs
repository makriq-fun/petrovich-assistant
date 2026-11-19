//! Чтение веб-страниц и поиск. HTTPS обязателен, частные адреса закрыты,
//! редиректы отключены: адрес проверяется один раз и не подменяется.

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
    timeout: Duration,
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

    pub fn with_search_proxy(
        timeout_ms: u64,
        max_bytes: usize,
        allow_private: bool,
        search_proxy: &str,
    ) -> Result<Self> {
        let build = || {
            let mut b = reqwest::Client::builder()
                .timeout(Duration::from_millis(timeout_ms))
                .connect_timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("petrovich-assistant/", env!("CARGO_PKG_VERSION")));
            if !search_proxy.trim().is_empty() {
                b = b.proxy(
                    reqwest::Proxy::all(search_proxy.trim())
                        .with_context(|| format!("search_proxy {search_proxy}"))?,
                );
            }
            b.build().context("HTTP-клиент web")
        };
        let client = build()?;
        let search_client = if search_proxy.trim().is_empty() {
            client.clone()
        } else {
            build()?
        };
        Ok(Self {
            client,
            search_client,
            search_api_key: String::new(),
            timeout: Duration::from_millis(timeout_ms),
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

    pub async fn search(&self, base: &str, query: &str) -> Result<Vec<SearchHit>> {
        if self.has_search_api() {
            match self.search_tavily(query).await {
                Ok(hits) => return Ok(hits),
                Err(e) if base.trim().is_empty() => return Err(e),
                Err(e) => tracing::warn!("Tavily: {e}; резервный поиск"),
            }
        }
        let parsed = Url::parse(base).context("search_url")?;
        if !self.allow_private {
            check_scheme(&parsed)?;
        }
        let kind = search_kind(&parsed);
        let bases = if kind == SearchKind::DuckDuckGo {
            vec![base.to_owned(), alternate_ddg_host(base)]
        } else {
            vec![base.to_owned()]
        };
        let attempt_timeout = if bases.len() > 1 {
            self.timeout / bases.len() as u32
        } else {
            self.timeout
        };
        let mut last_err = None;
        for (n, base) in bases.iter().enumerate() {
            if n > 0 {
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
            let mut url = Url::parse(base).context("search_url")?;
            {
                let mut q = url.query_pairs_mut();
                q.append_pair("q", query);
                if kind == SearchKind::Searxng {
                    q.append_pair("format", "json")
                        .append_pair("language", "ru");
                }
            }
            let req = self
                .search_client
                .get(url.clone())
                .timeout(attempt_timeout)
                .header(
                    reqwest::header::USER_AGENT,
                    "Mozilla/5.0 (X11; Linux aarch64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/121.0 Safari/537.36",
                )
                .header(
                    reqwest::header::ACCEPT,
                    "text/html,application/xhtml+xml,application/json;q=0.9,*/*;q=0.8",
                )
                .header(reqwest::header::ACCEPT_LANGUAGE, "ru-RU,ru;q=0.9,en;q=0.5");
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    last_err = Some(anyhow::anyhow!("поиск {url}: {e}"));
                    continue;
                }
            };
            if !resp.status().is_success() {
                last_err = Some(anyhow::anyhow!("поиск: HTTP {}", resp.status()));
                continue;
            }
            let bytes = match crate::http_read(resp, self.max_bytes).await {
                Ok(b) => b,
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            };
            let parsed = match kind {
                SearchKind::Searxng => parse_searxng(&bytes),
                SearchKind::DuckDuckGo => parse_duckduckgo(&bytes),
                SearchKind::DuckDuckGoLite => parse_duckduckgo_lite(&bytes),
            };
            match parsed {
                Ok(hits) => return Ok(hits),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("поиск: нет ответа")))
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchKind {
    Searxng,
    DuckDuckGo,
    DuckDuckGoLite,
}

fn alternate_ddg_host(base: &str) -> String {
    let Ok(mut url) = Url::parse(base) else {
        return base.to_owned();
    };
    let host = match url.host_str() {
        Some("html.duckduckgo.com") => "duckduckgo.com",
        Some("duckduckgo.com") => "html.duckduckgo.com",
        _ => return base.to_owned(),
    };
    if url.set_host(Some(host)).is_err() {
        return base.to_owned();
    }
    url.to_string()
}

fn search_kind(url: &Url) -> SearchKind {
    match url.host_str() {
        Some(h) if h.eq_ignore_ascii_case("lite.duckduckgo.com") => SearchKind::DuckDuckGoLite,
        Some(h)
            if h.eq_ignore_ascii_case("html.duckduckgo.com")
                || h.eq_ignore_ascii_case("duckduckgo.com") =>
        {
            SearchKind::DuckDuckGo
        }
        _ => SearchKind::Searxng,
    }
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

fn parse_searxng(bytes: &[u8]) -> Result<Vec<SearchHit>> {
    let v: serde_json::Value =
        serde_json::from_slice(bytes).context("поиск: не JSON (нужен SearXNG)")?;
    Ok(v.get("results")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
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
                .collect()
        })
        .unwrap_or_default())
}

fn parse_duckduckgo(bytes: &[u8]) -> Result<Vec<SearchHit>> {
    let text = String::from_utf8_lossy(bytes);
    if !text.contains("result__a") {
        bail!("поиск: не HTML DuckDuckGo");
    }
    let mut hits = Vec::new();
    let mut pos = 0;
    while let Some(rel) = text[pos..].find("class=\"result__a\"") {
        let i = pos + rel;
        pos = i + 1;
        let Some(a) = text[..i].rfind("<a ") else {
            continue;
        };
        let Some(gt) = text[i..].find('>') else {
            break;
        };
        let title_start = i + gt + 1;
        let Some(close) = text[title_start..].find("</a>") else {
            break;
        };
        let title_end = title_start + close;
        let title = crate::html::extract(&text[title_start..title_end], 300).1;
        let Some(href) = extract_href(&text[a..title_end]) else {
            continue;
        };
        let Some(url) = ddg_target(&href) else {
            continue;
        };
        let after = &text[title_end..];
        let snippet = if let Some(s) = after.find("class=\"result__snippet") {
            if let Some(gt) = after[s..].find('>') {
                let start = s + gt + 1;
                let end = after[start..]
                    .find("</a>")
                    .or_else(|| after[start..].find("</div>"))
                    .unwrap_or(after.len() - start);
                crate::html::extract(&after[start..start + end], 300).1
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        if !title.is_empty() {
            hits.push(SearchHit {
                title,
                url,
                snippet,
            });
        }
        if hits.len() == 5 {
            break;
        }
    }
    Ok(hits)
}

fn parse_duckduckgo_lite(bytes: &[u8]) -> Result<Vec<SearchHit>> {
    let text = String::from_utf8_lossy(bytes);
    if !text.contains("result-link") {
        bail!("поиск: не HTML DuckDuckGo Lite");
    }
    let mut hits = Vec::new();
    let mut pos = 0;
    while let Some(rel) = text[pos..].find("result-link") {
        let i = pos + rel;
        pos = i + 1;
        let Some(a) = text[..i].rfind("<a ") else {
            continue;
        };
        let Some(gt) = text[i..].find('>') else {
            break;
        };
        let title_start = i + gt + 1;
        let Some(close) = text[title_start..].find("</a>") else {
            break;
        };
        let title_end = title_start + close;
        let title = crate::html::extract(&text[title_start..title_end], 300).1;
        let Some(href) = extract_href(&text[a..title_end]) else {
            continue;
        };
        let Some(url) = ddg_target(&href) else {
            continue;
        };
        let after = &text[title_end..];
        let snippet = if let Some(s) = after
            .find("result-snippet\"")
            .or_else(|| after.find("result-snippet'"))
        {
            if let Some(gt) = after[s..].find('>') {
                let start = s + gt + 1;
                let end = after[start..].find("</td>").unwrap_or(after.len() - start);
                crate::html::extract(&after[start..start + end], 300).1
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        if !title.is_empty() {
            hits.push(SearchHit {
                title,
                url,
                snippet,
            });
        }
        if hits.len() == 5 {
            break;
        }
    }
    Ok(hits)
}

fn extract_href(fragment: &str) -> Option<String> {
    let start = fragment.find("href=\"")? + 6;
    let end = fragment[start..].find('"')? + start;
    let href = crate::html::decode(&fragment[start..end]);
    (!href.is_empty()).then_some(href)
}

fn ddg_target(href: &str) -> Option<String> {
    if href.starts_with("//duckduckgo.com/l/") || href.starts_with("https://duckduckgo.com/l/") {
        let full = if href.starts_with("//") {
            format!("https:{href}")
        } else {
            href.to_owned()
        };
        let url = Url::parse(&full).ok()?;
        return url
            .query_pairs()
            .find(|(k, _)| k == "uddg")
            .map(|(_, v)| v.into_owned());
    }
    if href.starts_with("https://") || href.starts_with("http://") {
        return Some(href.to_owned());
    }
    None
}

fn check_scheme(url: &Url) -> Result<()> {
    if url.scheme() != "https" {
        bail!("web: разрешён только HTTPS");
    }
    Ok(())
}

fn is_private_name(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
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

    #[tokio::test]
    async fn parses_searxng_results() {
        let (url, handle) = serve_once(
            200,
            "application/json",
            r#"{"results":[{"title":"A","url":"https://a","content":"one"},{"title":"B","url":"https://b","content":"two"}]}"#,
        )
        .await;
        let w = Web::new(5000, 100_000, true).unwrap();
        let hits = w.search(&url, "погода").await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "A");
        handle.await.unwrap();
    }

    #[test]
    fn parses_duckduckgo_results() {
        let html = r#"<a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fx%3D1&amp;rut=abc">Заголовок &amp; тест</a>
            <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa">Сниппет <b>тест</b></a>
            <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fb">Второй</a>
            <a class="result__snippet">Два</a>"#;
        let hits = parse_duckduckgo(html.as_bytes()).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Заголовок & тест");
        assert_eq!(hits[0].url, "https://example.com/a?x=1");
        assert_eq!(hits[0].snippet, "Сниппет тест");
        assert_eq!(hits[1].title, "Второй");
    }

    #[test]
    fn rejects_non_duckduckgo_html() {
        assert!(parse_duckduckgo(b"<html>challenge</html>").is_err());
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

    #[test]
    fn detects_duckduckgo_url() {
        let ddg = Url::parse("https://html.duckduckgo.com/html/").unwrap();
        let searx = Url::parse("https://searx.example/search").unwrap();
        assert!(matches!(search_kind(&ddg), SearchKind::DuckDuckGo));
        assert!(matches!(search_kind(&searx), SearchKind::Searxng));
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
