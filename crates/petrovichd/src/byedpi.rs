//! Встроенный обход блокировок YouTube (ByeDPI-подобный движок).
//!
//! Никаких дочерних процессов: локальный SOCKS5 слушает только loopback,
//! системные настройки не меняются. Desync применяется только к доменам
//! YouTube/Google Video, остальной трафик идёт прозрачно.
//!
//! Поддерживаются символические стратегии (`split_sni`, `disorder_sni`,
//! `oob_sni`, `fake_ttl_sni`, `split_N`) и аргументы в духе ByeByeDPI:
//! `-d N` (точка split, отрицательное — disorder), `-f` (fake с TTL),
//! `-o` (out-of-band), `-s` (split). Остальные флаги ByeByeDPI не влияют на
//! нашу реализацию и игнорируются.

use std::io;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use petrovich_core::config::ByeDpiConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SplitPos {
    Absolute(usize),
    Sni(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Split(usize),
    Disorder(usize),
    Oob(usize),
    FakeTtl(usize),
    /// Несколько разрезов ClientHello подряд: пресеты ByeByeDPI вида
    /// `-d1 -s1+s -s3+s ...`.
    Chain {
        parts: Vec<SplitPos>,
        /// Первый разрез делается в обратном порядке (ciadpi `-d`).
        disorder_first: bool,
    },
    Passthrough,
}

#[derive(Clone, Debug)]
struct Strategy {
    label: String,
    mode: Mode,
}

pub struct ByeDpi {
    port: u16,
    strategies: Vec<Strategy>,
    active: Arc<AtomicUsize>,
    state_path: PathBuf,
    refresh_s: u64,
    probe_timeout: Duration,
    probe_video: String,
}

impl ByeDpi {
    pub async fn start(cfg: &ByeDpiConfig, state_path: PathBuf) -> Result<Arc<Self>> {
        let strategies: Vec<Strategy> = cfg.strategies.iter().map(|s| parse_strategy(s)).collect();
        if strategies.is_empty() {
            bail!("byedpi: нет стратегий");
        }
        let listener = TcpListener::bind(("127.0.0.1", cfg.listen_port))
            .await
            .context("byedpi: loopback listener")?;
        let port = listener.local_addr()?.port();
        let app = Arc::new(Self {
            port,
            strategies,
            active: Arc::new(AtomicUsize::new(0)),
            state_path,
            refresh_s: cfg.refresh_s.max(30),
            probe_timeout: Duration::from_millis(cfg.probe_timeout_ms.max(500)),
            probe_video: cfg.probe_video.clone(),
        });
        let accept_app = app.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((sock, _)) => {
                        let app = accept_app.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_client(sock, app).await {
                                tracing::debug!("byedpi: соединение: {e:#}");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("byedpi: accept: {e}");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        });
        app.restore();
        tracing::info!("ByeDPI: socks5h://127.0.0.1:{port}");
        Ok(app)
    }

    pub fn proxy_url(&self) -> String {
        format!("socks5h://127.0.0.1:{}", self.port)
    }

    /// Прозрачный режим: SOCKS-посредник не нужен, YouTube идёт напрямую.
    /// Так обход не ломает сеть, где провайдер и так отдаёт YouTube.
    pub fn is_direct(&self) -> bool {
        self.current_mode() == Mode::Passthrough
    }

    pub fn current_label(&self) -> String {
        let i = self.active.load(Ordering::Acquire);
        self.strategies
            .get(i)
            .map(|s| s.label.clone())
            .unwrap_or_else(|| "passthrough".into())
    }

    fn set(&self, index: usize) {
        self.active
            .store(index.min(self.strategies.len()), Ordering::Release);
    }

    fn current_mode(&self) -> Mode {
        self.strategies
            .get(self.active.load(Ordering::Acquire))
            .map(|s| s.mode.clone())
            .unwrap_or(Mode::Passthrough)
    }

    fn restore(&self) {
        let Ok(bytes) = std::fs::read(&self.state_path) else {
            return;
        };
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return;
        };
        let Some(label) = v.get("strategy").and_then(|x| x.as_str()) else {
            return;
        };
        if let Some(i) = self.strategies.iter().position(|s| s.label == label) {
            self.set(i);
            tracing::info!("ByeDPI: сохранённая стратегия «{label}»");
        }
    }

    fn persist(&self) {
        let path = &self.state_path;
        let value = serde_json::json!({ "strategy": self.current_label() });
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("tmp");
        match std::fs::write(&tmp, serde_json::to_vec(&value).unwrap_or_default())
            .and_then(|_| std::fs::rename(&tmp, path))
        {
            Ok(()) => tracing::info!("ByeDPI: выбрана «{}»", self.current_label()),
            Err(e) => tracing::warn!("byedpi: не сохранил выбор: {e}"),
        }
    }

    /// Проверяет выбранную стратегию, при ошибке перебирает остальные.
    /// `probe` должен реально открыть медиапоток через локальный прокси.
    pub async fn probe<F, Fut>(&self, probe: F) -> Result<String>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<String>>,
    {
        let saved = self.active.load(Ordering::Acquire);
        let order =
            std::iter::once(saved).chain((0..self.strategies.len()).filter(move |i| *i != saved));
        for i in order {
            self.set(i);
            let label = self.current_label();
            tracing::info!("ByeDPI: проверяю «{label}»");
            match tokio::time::timeout(self.probe_timeout, probe()).await {
                Ok(Ok(detail)) => {
                    self.persist();
                    tracing::info!("ByeDPI: работает «{label}»: {detail}");
                    return Ok(label);
                }
                Ok(Err(e)) => tracing::warn!("ByeDPI: «{label}» не работает: {e:#}"),
                Err(_) => tracing::warn!(
                    "ByeDPI: «{label}» не ответил за {} мс",
                    self.probe_timeout.as_millis()
                ),
            }
        }
        // Прямой путь: прокси остаётся, но ничего не трогает.
        self.set(self.strategies.len());
        self.persist();
        tracing::warn!("ByeDPI: ни одна стратегия не сработала; прозрачный режим");
        Ok("passthrough".into())
    }

    pub async fn refresh_loop<F, Fut>(&self, probe: F, busy: Arc<dyn Fn() -> bool + Send + Sync>)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<String>>,
    {
        loop {
            // Passthrough после неудачного старта (сеть ещё не поднялась)
            // перепроверяем чаще, чтобы YouTube заработал без перезапуска.
            let passthrough = self.current_mode() == Mode::Passthrough;
            let wait = if passthrough {
                self.refresh_s.min(30)
            } else {
                self.refresh_s
            };
            tokio::time::sleep(Duration::from_secs(wait)).await;
            if busy() {
                continue;
            }
            if passthrough {
                tracing::info!("ByeDPI: повторная проверка после passthrough");
                let _ = self.probe(&probe).await;
                continue;
            }
            let selected = self.active.load(Ordering::Acquire);
            self.set(selected);
            let alive = matches!(
                tokio::time::timeout(self.probe_timeout, probe()).await,
                Ok(Ok(_))
            );
            if alive {
                continue;
            }
            tracing::warn!("ByeDPI: стратегия перестала работать, перебираю заново");
            let _ = self.probe(&probe).await;
        }
    }

    pub fn probe_video(&self) -> &str {
        &self.probe_video
    }
}

fn parse_strategy(raw: &str) -> Strategy {
    let label = raw.trim().to_owned();
    let low = label.to_lowercase();
    let symbolic = match low.as_str() {
        "split_sni" | "split" => Some(Mode::Split(0)),
        "disorder_sni" | "disorder" => Some(Mode::Disorder(0)),
        "oob_sni" | "oob" => Some(Mode::Oob(0)),
        "fake_ttl_sni" | "fake" => Some(Mode::FakeTtl(0)),
        "passthrough" | "none" | "direct" => Some(Mode::Passthrough),
        _ => None,
    };
    if let Some(mode) = symbolic {
        return Strategy { label, mode };
    }
    if let Some(rest) = low.strip_prefix("split_") {
        if let Ok(n) = rest.parse::<usize>() {
            return Strategy {
                label,
                mode: Mode::Split(n.max(1)),
            };
        }
    }

    // Полный синтаксис ciadpi: -d <pos_t> и -s <pos_t>, допускается
    // несколько директив. pos_t: offset[+s], где +s — смещение от SNI.
    let tokens: Vec<&str> = label.split_whitespace().collect();
    let mut parts = Vec::new();
    let mut disorder_first = false;
    let mut simple = (0usize, false, false, false); // split, fake, oob, disorder
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        let (kind, value) = if tok == "-d" || tok == "--disorder" {
            i += 1;
            ("d", tokens.get(i).copied())
        } else if tok == "-s" || tok == "--split" {
            i += 1;
            ("s", tokens.get(i).copied())
        } else if let Some(v) = tok.strip_prefix("-d") {
            if v.is_empty() {
                ("d", None)
            } else {
                ("d", Some(v))
            }
        } else if let Some(v) = tok.strip_prefix("-s") {
            if v.is_empty() {
                ("s", None)
            } else {
                ("s", Some(v))
            }
        } else if tok == "-f" || tok == "--fake" {
            ("f", None)
        } else if tok == "-o" || tok == "--oob" {
            ("o", None)
        } else {
            i += 1;
            continue;
        };
        if kind == "f" {
            simple.1 = true;
            i += 1;
            continue;
        }
        if kind == "o" {
            simple.2 = true;
            i += 1;
            continue;
        }
        let Some(v) = value else {
            simple.0 = simple.0.max(1);
            i += 1;
            continue;
        };
        let (num, from_sni) = split_pos(v);
        if kind == "d" {
            if v.starts_with('-') {
                simple.3 = true;
                simple.0 = num;
            } else if parts.is_empty() && simple.0 == 0 {
                simple.0 = num;
                disorder_first = true;
            } else {
                disorder_first = true;
                parts.push(SplitPos::Absolute(num));
            }
        } else if from_sni || !parts.is_empty() {
            parts.push(if from_sni {
                SplitPos::Sni(num)
            } else {
                SplitPos::Absolute(num)
            });
        } else {
            simple.0 = simple.0.max(num.max(1));
        }
        i += 1;
    }

    let mode = if parts.len() >= 2 {
        if disorder_first {
            parts.insert(0, SplitPos::Absolute(simple.0.max(1)));
        }
        Mode::Chain {
            parts,
            disorder_first,
        }
    } else if simple.3 {
        Mode::Disorder(simple.0.max(1))
    } else if simple.2 {
        Mode::Oob(simple.0)
    } else if simple.1 {
        Mode::FakeTtl(simple.0)
    } else if simple.0 > 0 {
        Mode::Split(simple.0)
    } else {
        Mode::Split(0)
    };
    Strategy { label, mode }
}

fn split_pos(value: &str) -> (usize, bool) {
    let value = value.trim();
    let from_sni = value.ends_with("+s");
    let base = value.split('+').next().unwrap_or("0");
    let n = base
        .parse::<i32>()
        .ok()
        .map(|n| n.unsigned_abs() as usize)
        .unwrap_or(0);
    (n, from_sni)
}

async fn handle_client(mut client: TcpStream, app: Arc<ByeDpi>) -> Result<()> {
    let mut head = [0u8; 2];
    client.read_exact(&mut head).await?;
    if head[0] != 5 {
        bail!("не SOCKS5");
    }
    let mut methods = vec![0u8; head[1] as usize];
    client.read_exact(&mut methods).await?;
    client.write_all(&[5, 0]).await?;
    let mut req = [0u8; 4];
    client.read_exact(&mut req).await?;
    if req[0] != 5 || req[1] != 1 {
        client.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        bail!("поддерживается только CONNECT");
    }
    let host = match req[3] {
        1 => {
            let mut ip = [0u8; 4];
            client.read_exact(&mut ip).await?;
            IpAddr::from(ip).to_string()
        }
        4 => {
            let mut ip = [0u8; 16];
            client.read_exact(&mut ip).await?;
            IpAddr::from(ip).to_string()
        }
        3 => {
            let len = {
                let mut b = [0u8; 1];
                client.read_exact(&mut b).await?;
                b[0] as usize
            };
            let mut name = vec![0u8; len];
            client.read_exact(&mut name).await?;
            String::from_utf8_lossy(&name).into_owned()
        }
        _ => bail!("неизвестный ATYP"),
    };
    let mut port = [0u8; 2];
    client.read_exact(&mut port).await?;
    let port = u16::from_be_bytes(port);
    let mut remote = match connect_ipv4_first(&host, port).await {
        Ok(s) => s,
        Err(e) => {
            client.write_all(&[5, 4, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
            return Err(e).context("byedpi: connect");
        }
    };
    client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    // Без TCP_NODELAY мелкие части ClientHello склеиваются в один сегмент,
    // и DPI видит целый hello, как при passthrough.
    let _ = remote.set_nodelay(true);
    let mode = if is_youtube(&host) {
        app.current_mode()
    } else {
        Mode::Passthrough
    };
    // Первый пакет клиента — TLS ClientHello или HTTP-запрос. Ждём его
    // коротко; если сервер заговорил первым, просто соединяем потоки.
    let mut first = vec![0u8; 16 * 1024];
    let n = match tokio::time::timeout(Duration::from_millis(1500), client.read(&mut first)).await {
        Ok(Ok(n)) => n,
        _ => 0,
    };
    if n > 0 && mode != Mode::Passthrough {
        first.truncate(n);
        send_desync(&mut remote, &first, mode).await?;
    } else if n > 0 {
        remote.write_all(&first[..n]).await?;
    }
    tokio::io::copy_bidirectional(&mut client, &mut remote).await?;
    Ok(())
}

/// На Мини 2 нет IPv6-маршрута, а DNS может отдать AAAA первым. Пробуем
/// IPv4, затем IPv6, вместо того чтобы падать на первом недоступном адресе.
async fn connect_ipv4_first(host: &str, port: u16) -> std::io::Result<TcpStream> {
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host, port)).await?.collect();
    let (v4, v6): (Vec<_>, Vec<_>) = addrs.into_iter().partition(|a| a.is_ipv4());
    let mut last = None;
    for addr in v4.into_iter().chain(v6) {
        match TcpStream::connect(addr).await {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("нет адресов")))
}

fn is_youtube(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    const SUFFIXES: &[&str] = &[
        "youtube.com",
        "youtu.be",
        "youtube-nocookie.com",
        "googlevideo.com",
        "ytimg.com",
        "ggpht.com",
        "youtubei.googleapis.com",
        "googlesyndication.com",
        "googleadservices.com",
        "doubleclick.net",
    ];
    SUFFIXES
        .iter()
        .any(|s| h == *s || h.ends_with(&format!(".{s}")))
}

/// Позиция имени сервера в TLS ClientHello, если удалось разобрать.
fn sni_offset(data: &[u8]) -> Option<usize> {
    if data.len() < 44 || data[0] != 0x16 {
        return None;
    }
    let rec_len = u16::from_be_bytes([data[3], data[4]]) as usize;
    if 5 + rec_len > data.len() {
        return None;
    }
    let mut p = 5usize;
    if data[p] != 1 {
        return None;
    }
    p += 4;
    p += 2 + 32; // version + random
    let sid = *data.get(p)? as usize;
    p += 1 + sid;
    let cs = u16::from_be_bytes([*data.get(p)?, *data.get(p + 1)?]) as usize;
    p += 2 + cs;
    let comp = *data.get(p)? as usize;
    p += 1 + comp;
    let ext_len = u16::from_be_bytes([*data.get(p)?, *data.get(p + 1)?]) as usize;
    p += 2;
    let end = (p + ext_len).min(data.len());
    while p + 4 <= end {
        let kind = u16::from_be_bytes([data[p], data[p + 1]]);
        let len = u16::from_be_bytes([data[p + 2], data[p + 3]]) as usize;
        p += 4;
        if p + len > end {
            break;
        }
        if kind == 0 {
            // server_name: list_len(2) + type(1) + name_len(2) + name
            let q = p + 3;
            let name_len = u16::from_be_bytes([data[q], data[q + 1]]) as usize;
            if q + 2 + name_len <= p + len {
                return Some(q + 2);
            }
        }
        p += len;
    }
    None
}

fn split_at(data: &[u8], n: usize, sni: usize) -> usize {
    if n == 0 {
        sni
    } else {
        n.min(data.len().saturating_sub(1))
    }
}

async fn send_desync(remote: &mut TcpStream, first: &[u8], mode: Mode) -> Result<()> {
    let sni = sni_offset(first).unwrap_or(1);
    match mode {
        Mode::Split(n) => {
            let at = split_at(first, n, sni);
            remote.write_all(&first[..at]).await?;
            remote.flush().await?;
            remote.write_all(&first[at..]).await?;
        }
        Mode::Disorder(n) => {
            // Как ciadpi: сегмент с TTL 1 виден DPI, но не доходит до сервера,
            // затем тот же ClientHello уходит целиком обычным TTL.
            let at = split_at(first, n, sni);
            set_ttl(remote, 1);
            let _ = remote.write_all(&first[..at]).await;
            let _ = remote.flush().await;
            tokio::time::sleep(Duration::from_millis(1)).await;
            set_ttl(remote, 64);
            remote.write_all(first).await?;
        }
        Mode::Oob(n) => {
            let at = split_at(first, n, sni).clamp(1, first.len());
            remote.write_all(&first[..at - 1]).await?;
            remote.flush().await?;
            send_oob(remote, first[at - 1]).await;
            // Байт остаётся и в обычном потоке: если TCP не поддерживает
            // OOB, сервер всё равно получит целый TLS-рекорд.
            remote.write_all(&first[at - 1..]).await?;
        }
        Mode::FakeTtl(n) => {
            let at = split_at(first, n, sni);
            let fake = fake_hello(first);
            set_ttl(remote, 1);
            let _ = remote.write_all(&fake).await;
            let _ = remote.flush().await;
            set_ttl(remote, 64);
            tokio::time::sleep(Duration::from_millis(1)).await;
            remote.write_all(&first[..at]).await?;
            remote.flush().await?;
            remote.write_all(&first[at..]).await?;
        }
        Mode::Chain {
            parts,
            disorder_first,
        } => {
            let mut cuts: Vec<usize> = parts
                .iter()
                .map(|p| match p {
                    SplitPos::Absolute(n) => *n,
                    SplitPos::Sni(n) => sni.saturating_add(*n),
                })
                .filter(|n| *n > 0 && *n < first.len())
                .collect();
            cuts.sort_unstable();
            cuts.dedup();
            let mut prev = 0usize;
            if disorder_first {
                // ciadpi: первый сегмент уходит с TTL 1 и больше не повторяется,
                // остальные части начинаются сразу за ним.
                prev = cuts.first().copied().unwrap_or(sni);
                set_ttl(remote, 1);
                let _ = remote.write_all(&first[..prev]).await;
                let _ = remote.flush().await;
                tokio::time::sleep(Duration::from_millis(30)).await;
                set_ttl(remote, 64);
            }
            let rest = if disorder_first && !cuts.is_empty() {
                &cuts[1..]
            } else {
                &cuts[..]
            };
            for &cut in rest {
                remote.write_all(&first[prev..cut]).await?;
                remote.flush().await?;
                prev = cut;
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            remote.write_all(&first[prev..]).await?;
        }
        Mode::Passthrough => remote.write_all(first).await?,
    }
    Ok(())
}

fn fake_hello(real: &[u8]) -> Vec<u8> {
    let mut fake = real.to_vec();
    if let Some(sni) = sni_offset(real) {
        let name = b"example.com";
        for (i, b) in fake[sni..].iter_mut().enumerate() {
            if i < name.len() {
                *b = name[i];
            }
        }
    }
    if fake.len() > 11 {
        fake[11] ^= 0x5a;
    }
    fake
}

async fn send_oob(stream: &TcpStream, byte: u8) {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();
    // SAFETY: fd валиден, буфер живёт до вызова.
    let rc = unsafe {
        libc::send(
            fd,
            &byte as *const u8 as *const libc::c_void,
            1,
            libc::MSG_OOB | libc::MSG_DONTWAIT,
        )
    };
    if rc < 0 {
        tracing::debug!("byedpi: OOB: {}", io::Error::last_os_error());
    }
}

fn set_ttl(stream: &TcpStream, ttl: i32) {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();
    let len = std::mem::size_of::<i32>() as libc::socklen_t;
    // SAFETY: fd валиден, ttl — i32, размер передан верно. Сокет может быть
    // IPv4 или IPv6 (в том числе IPv4-mapped), поэтому ставим обе опции.
    unsafe {
        let v4 = libc::setsockopt(
            fd,
            libc::IPPROTO_IP,
            libc::IP_TTL,
            &ttl as *const i32 as *const libc::c_void,
            len,
        );
        let v6 = libc::setsockopt(
            fd,
            libc::IPPROTO_IPV6,
            libc::IPV6_UNICAST_HOPS,
            &ttl as *const i32 as *const libc::c_void,
            len,
        );
        if v4 != 0 && v6 != 0 {
            tracing::debug!("byedpi: TTL {ttl}: {}", io::Error::last_os_error());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_byebye_presets() {
        assert_eq!(parse_strategy("-d 1 -f -1 -t 1").mode, Mode::FakeTtl(1));
        assert_eq!(parse_strategy("-d -2 -f -1").mode, Mode::Disorder(2));
        assert_eq!(parse_strategy("-d 4 -s 1").mode, Mode::Split(4));
        assert_eq!(parse_strategy("-d 1 -o 1").mode, Mode::Oob(1));
        assert_eq!(parse_strategy("split_sni").mode, Mode::Split(0));
        assert_eq!(parse_strategy("split_16").mode, Mode::Split(16));
        assert!(matches!(
            parse_strategy("-d1 -s1+s -s3+s -s6+s -s12+s -s14+s -s20+s -s24+s -s30+s -a1").mode,
            Mode::Chain { parts, disorder_first: true } if parts.len() == 9
        ));
    }

    #[test]
    fn recognizes_youtube_hosts_only() {
        assert!(is_youtube("rr1---sn-abc.googlevideo.com"));
        assert!(is_youtube("www.youtube.com"));
        assert!(is_youtube("youtu.be"));
        assert!(!is_youtube("example.com"));
        assert!(!is_youtube("notyoutube.com"));
    }

    #[test]
    fn parses_sni_of_real_client_hello() {
        let host = b"www.youtube.com";
        let mut data = vec![
            0x00,
            (host.len() + 3) as u8, // список server_name
            0x00,                   // host_name
            0x00,
            host.len() as u8,
        ];
        data.extend_from_slice(host);
        let mut ext = vec![0x00, 0x00]; // расширение server_name
        ext.extend_from_slice(&(data.len() as u16).to_be_bytes());
        ext.extend_from_slice(&data);
        let mut body = vec![0x03, 0x03];
        body.extend_from_slice(&[0u8; 32]);
        body.push(0); // session id
        body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]); // cipher
        body.push(1);
        body.push(0); // compression
        body.extend_from_slice(&(ext.len() as u16).to_be_bytes());
        body.extend_from_slice(&ext);
        let mut hs = vec![0x01, 0, 0, body.len() as u8];
        hs.extend_from_slice(&body);
        let mut rec = vec![0x16, 0x03, 0x01];
        rec.extend_from_slice(&(hs.len() as u16).to_be_bytes());
        rec.extend_from_slice(&hs);
        assert_eq!(sni_offset(&rec), Some(rec.len() - host.len()));
    }

    #[tokio::test]
    async fn socks5_tunnel_carries_a_local_stream() {
        use tokio::io::AsyncReadExt;
        let echo = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let echo_addr = echo.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = echo.accept().await.unwrap();
            let mut buf = [0u8; 5];
            sock.read_exact(&mut buf).await.unwrap();
            sock.write_all(&buf).await.unwrap();
        });
        let dir = std::env::temp_dir().join(format!("petrovich-byedpi-{}", std::process::id()));
        let cfg = ByeDpiConfig {
            listen_port: 0,
            ..ByeDpiConfig::default()
        };
        let app = ByeDpi::start(&cfg, dir.clone()).await.unwrap();
        let mut proxy = TcpStream::connect(("127.0.0.1", app.port)).await.unwrap();
        proxy.write_all(&[5, 1, 0]).await.unwrap();
        let mut sel = [0u8; 2];
        proxy.read_exact(&mut sel).await.unwrap();
        assert_eq!(sel, [5, 0]);
        let mut req = vec![5, 1, 0, 1];
        req.extend_from_slice(
            &echo_addr
                .ip()
                .to_string()
                .parse::<std::net::Ipv4Addr>()
                .unwrap()
                .octets(),
        );
        req.extend_from_slice(&echo_addr.port().to_be_bytes());
        proxy.write_all(&req).await.unwrap();
        let mut reply = [0u8; 10];
        proxy.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], 0);
        proxy.write_all(b"hello").await.unwrap();
        let mut got = [0u8; 5];
        proxy.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"hello");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn desync_preserves_the_tcp_stream() {
        use tokio::io::AsyncReadExt;
        let payload: Vec<u8> = (0..=127u8).collect();
        for mode in [Mode::Split(2), Mode::Oob(2)] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let addr = listener.local_addr().unwrap();
            let expected = payload.clone();
            let server = tokio::spawn(async move {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut got = vec![0u8; expected.len()];
                sock.read_exact(&mut got).await.unwrap();
                // Disorder шлёт копию с TTL 1; на loopback она доходит,
                // в реальной сети гаснет. Поток сервера всегда заканчивается
                // исходными байтами.
                assert!(got.ends_with(&expected));
            });
            let mut client = TcpStream::connect(addr).await.unwrap();
            send_desync(&mut client, &payload, mode).await.unwrap();
            server.await.unwrap();
        }
    }
}
