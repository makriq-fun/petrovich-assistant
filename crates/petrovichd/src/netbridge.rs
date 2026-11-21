//! Локальный HTTP CONNECT-мост к SOCKS5 для librespot.
//!
//! librespot умеет только HTTP CONNECT, но не SOCKS5. Мост принимает
//! CONNECT на loopback, открывает SOCKS5-туннель и прозрачно проксирует
//! TCP. Отдельный процесс не нужен, системные настройки не меняются.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

const MAX_HEADER: usize = 8192;

pub struct HttpConnectBridge {
    url: String,
    task: JoinHandle<()>,
}

impl HttpConnectBridge {
    /// Поднимает мост к `socks5h://host:port` и возвращает URL вида
    /// `http://127.0.0.1:<port>`, который можно отдать librespot.
    pub async fn start(socks: &str) -> Result<Self> {
        let proxy = ParsedProxy::parse(socks)?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .context("HTTP CONNECT мост: bind")?;
        let addr = listener.local_addr().context("HTTP CONNECT мост: адрес")?;
        let proxy = Arc::new(proxy);
        let task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((socket, _)) => {
                        let proxy = proxy.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle(socket, &proxy).await {
                                tracing::warn!("HTTP CONNECT мост: {e:#}");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("HTTP CONNECT мост: accept: {e}");
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
            }
        });
        Ok(Self {
            url: format!("http://127.0.0.1:{}", addr.port()),
            task,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }
}

impl Drop for HttpConnectBridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct ParsedProxy {
    host: String,
    port: u16,
    user: Option<String>,
    password: Option<String>,
}

impl ParsedProxy {
    fn parse(raw: &str) -> Result<Self> {
        let url = url::Url::parse(raw).context("SOCKS5 URL")?;
        if !matches!(url.scheme(), "socks5" | "socks5h") {
            bail!("мост поддерживает только socks5:// и socks5h://");
        }
        let host = url.host_str().context("SOCKS5: нет хоста")?.to_owned();
        let port = url.port().unwrap_or(1080);
        let user = (!url.username().is_empty()).then(|| url.username().to_owned());
        let password = url.password().map(str::to_owned);
        Ok(Self {
            host,
            port,
            user,
            password,
        })
    }
}

async fn handle(mut client: TcpStream, proxy: &ParsedProxy) -> Result<()> {
    let (host, port, rest) = read_connect(&mut client).await?;
    let mut upstream = socks_connect(proxy, &host, port).await?;
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .context("CONNECT: ответ клиенту")?;
    if !rest.is_empty() {
        upstream
            .write_all(&rest)
            .await
            .context("CONNECT: первые данные")?;
    }
    tokio::io::copy_bidirectional(&mut client, &mut upstream)
        .await
        .context("CONNECT: обмен")?;
    Ok(())
}

/// Читает HTTP CONNECT вместе с уже прочитанным «хвостом» после заголовков.
async fn read_connect(socket: &mut TcpStream) -> Result<(String, u16, Vec<u8>)> {
    let mut buf = Vec::with_capacity(1024);
    loop {
        if buf.len() >= MAX_HEADER {
            bail!("CONNECT: заголовок больше {MAX_HEADER} байт");
        }
        let mut chunk = [0u8; 1024];
        let n = socket.read(&mut chunk).await.context("CONNECT: чтение")?;
        if n == 0 {
            bail!("CONNECT: соединение закрыто до заголовка");
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = find_header_end(&buf) {
            let header = std::str::from_utf8(&buf[..end]).context("CONNECT: заголовок не UTF-8")?;
            let (host, port) = parse_connect_line(header)?;
            return Ok((host, port, buf[end + 4..].to_vec()));
        }
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_connect_line(header: &str) -> Result<(String, u16)> {
    let line = header.lines().next().context("CONNECT: пустой запрос")?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().context("CONNECT: нет цели")?;
    if !method.eq_ignore_ascii_case("CONNECT") {
        bail!("ожидался CONNECT, получено {method}");
    }
    let (host, port) = target.rsplit_once(':').context("CONNECT: цель без порта")?;
    let host = host.trim_matches(['[', ']']).to_owned();
    let port = port.parse::<u16>().context("CONNECT: порт")?;
    Ok((host, port))
}

async fn socks_connect(proxy: &ParsedProxy, host: &str, port: u16) -> Result<TcpStream> {
    let mut stream = TcpStream::connect((proxy.host.as_str(), proxy.port))
        .await
        .with_context(|| format!("SOCKS5: подключение к {}:{}", proxy.host, proxy.port))?;
    let use_auth = proxy.user.is_some();
    let methods: &[u8] = if use_auth { &[0x00, 0x02] } else { &[0x00] };
    let mut greeting = Vec::with_capacity(2 + methods.len());
    greeting.extend_from_slice(&[0x05, methods.len() as u8]);
    greeting.extend_from_slice(methods);
    stream
        .write_all(&greeting)
        .await
        .context("SOCKS5: приветствие")?;
    let mut reply = [0u8; 2];
    stream
        .read_exact(&mut reply)
        .await
        .context("SOCKS5: ответ")?;
    if reply[0] != 0x05 {
        bail!("SOCKS5: версия {}", reply[0]);
    }
    match reply[1] {
        0x00 => {}
        0x02 => socks_auth(&mut stream, proxy).await?,
        method => bail!("SOCKS5: сервер выбрал метод {method:#04x}"),
    }
    let mut request = vec![0x05, 0x01, 0x00];
    request.extend(address_bytes(host));
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).await.context("SOCKS5: запрос")?;
    let mut head = [0u8; 4];
    stream
        .read_exact(&mut head)
        .await
        .context("SOCKS5: ответ на CONNECT")?;
    if head[0] != 0x05 || head[1] != 0x00 {
        bail!("SOCKS5: CONNECT отклонён ({:#04x})", head[1]);
    }
    let skip = match head[3] {
        0x01 => 4,
        0x04 => 16,
        0x03 => {
            let mut len = [0u8; 1];
            stream
                .read_exact(&mut len)
                .await
                .context("SOCKS5: длина host")?;
            usize::from(len[0])
        }
        atyp => bail!("SOCKS5: неизвестный ATYP {atyp}"),
    };
    let mut tail = vec![0u8; skip + 2];
    stream
        .read_exact(&mut tail)
        .await
        .context("SOCKS5: адрес")?;
    Ok(stream)
}

fn address_bytes(host: &str) -> Vec<u8> {
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        let mut out = vec![0x01];
        out.extend_from_slice(&ip.octets());
        out
    } else if let Ok(ip) = host.parse::<Ipv6Addr>() {
        let mut out = vec![0x04];
        out.extend_from_slice(&ip.octets());
        out
    } else {
        let bytes = host.as_bytes();
        let len = bytes.len().min(255);
        let mut out = vec![0x03, len as u8];
        out.extend_from_slice(&bytes[..len]);
        out
    }
}

async fn socks_auth(stream: &mut TcpStream, proxy: &ParsedProxy) -> Result<()> {
    let user = proxy.user.as_deref().unwrap_or("");
    let password = proxy.password.as_deref().unwrap_or("");
    let mut out = Vec::with_capacity(3 + user.len() + password.len());
    out.push(0x01);
    out.push(user.len().min(255) as u8);
    out.extend_from_slice(&user.as_bytes()[..user.len().min(255)]);
    out.push(password.len().min(255) as u8);
    out.extend_from_slice(&password.as_bytes()[..password.len().min(255)]);
    stream.write_all(&out).await.context("SOCKS5: auth")?;
    let mut reply = [0u8; 2];
    stream
        .read_exact(&mut reply)
        .await
        .context("SOCKS5: auth ответ")?;
    if reply[1] != 0x00 {
        bail!("SOCKS5: неверный логин или пароль");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ipv4_and_domain_connect_targets() {
        assert_eq!(
            parse_connect_line("CONNECT example.com:443 HTTP/1.1").unwrap(),
            ("example.com".into(), 443)
        );
        assert_eq!(
            parse_connect_line("CONNECT [::1]:443 HTTP/1.1").unwrap(),
            ("::1".into(), 443)
        );
        assert_eq!(address_bytes("1.2.3.4"), vec![1, 1, 2, 3, 4]);
        assert_eq!(address_bytes("example.com")[0], 3);
        assert_eq!(address_bytes("::1")[0], 4);
    }

    #[tokio::test]
    async fn bridges_connect_to_socks5_echo() {
        let echo = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let echo_addr = echo.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = echo.accept().await.unwrap();
            let mut b = [0u8; 4];
            s.read_exact(&mut b).await.unwrap();
            s.write_all(&b).await.unwrap();
        });

        let socks = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let socks_addr = socks.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut c, _) = socks.accept().await.unwrap();
            let mut greeting = [0u8; 2];
            c.read_exact(&mut greeting).await.unwrap();
            let mut methods = vec![0u8; greeting[1] as usize];
            c.read_exact(&mut methods).await.unwrap();
            c.write_all(&[5, 0]).await.unwrap();
            let mut head = [0u8; 4];
            c.read_exact(&mut head).await.unwrap();
            let mut addr = vec![
                0u8;
                match head[3] {
                    1 => 4,
                    4 => 16,
                    3 => {
                        let mut n = [0u8; 1];
                        c.read_exact(&mut n).await.unwrap();
                        n[0] as usize
                    }
                    _ => panic!("atyp"),
                }
            ];
            c.read_exact(&mut addr).await.unwrap();
            let mut port = [0u8; 2];
            c.read_exact(&mut port).await.unwrap();
            let mut target = TcpStream::connect((Ipv4Addr::LOCALHOST, echo_addr.port()))
                .await
                .unwrap();
            c.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
            tokio::io::copy_bidirectional(&mut c, &mut target)
                .await
                .unwrap();
        });

        let bridge = HttpConnectBridge::start(&format!("socks5h://{socks_addr}"))
            .await
            .unwrap();
        let port = url::Url::parse(bridge.url()).unwrap().port().unwrap();
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        client
            .write_all(
                format!("CONNECT 127.0.0.1:{} HTTP/1.1\r\n\r\n", echo_addr.port()).as_bytes(),
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        loop {
            let mut b = [0u8; 1];
            client.read_exact(&mut b).await.unwrap();
            response.push(b[0]);
            if response.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 200"));
        client.write_all(b"ping").await.unwrap();
        let mut echo = [0u8; 4];
        client.read_exact(&mut echo).await.unwrap();
        assert_eq!(&echo, b"ping");
    }

    #[test]
    fn parses_socks_url() {
        let p = ParsedProxy::parse("socks5h://user:pass@127.0.0.1:1080").unwrap();
        assert_eq!(p.host, "127.0.0.1");
        assert_eq!(p.port, 1080);
        assert_eq!(p.user.as_deref(), Some("user"));
        assert_eq!(p.password.as_deref(), Some("pass"));
        assert!(ParsedProxy::parse("http://127.0.0.1:8080").is_err());
    }
}
