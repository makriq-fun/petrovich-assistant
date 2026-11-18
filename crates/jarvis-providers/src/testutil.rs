//! Мини-сервер для интеграционных тестов провайдеров.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Отвечает один раз; возвращает базовый URL и задачу сервера.
pub async fn serve_once(
    status: u16,
    content_type: &str,
    body: &str,
) -> (String, tokio::task::JoinHandle<()>) {
    serve_once_bytes(status, content_type, body.as_bytes().to_vec()).await
}

pub async fn serve_once_bytes(
    status: u16,
    content_type: &str,
    body: Vec<u8>,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let content_type = content_type.to_owned();
    let handle = tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let mut need = None;
        loop {
            match sock.read(&mut tmp).await {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if need.is_none() {
                        if let Some(pos) = find_headers_end(&buf) {
                            let headers = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                            let len = headers
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            need = Some(pos + len);
                        }
                    }
                    if need.is_some_and(|n| buf.len() >= n) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            401 => "Unauthorized",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            _ => "Status",
        };
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = sock.write_all(head.as_bytes()).await;
        let _ = sock.write_all(&body).await;
        let _ = sock.shutdown().await;
    });
    (format!("http://{addr}"), handle)
}

fn find_headers_end(b: &[u8]) -> Option<usize> {
    b.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}
