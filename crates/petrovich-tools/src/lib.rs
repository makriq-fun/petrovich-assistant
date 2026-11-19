//! Инструменты ассистента и проверка разрешений.

pub mod html;
pub mod nightscout;
pub mod registry;
pub mod timers;
pub mod weather;
pub mod web;

#[cfg(test)]
mod testutil;

pub use nightscout::Nightscout;
pub use registry::{ControlAction, ToolContext, ToolResult};
pub use timers::{AlarmFired, AlarmInfo, AlarmManager, TimerFired, TimerInfo, TimerManager};
pub use weather::Weather;
pub use web::Web;

use anyhow::{bail, Result};
use futures_util::StreamExt;

/// Читает тело ответа с ограничением размера.
pub(crate) async fn http_read(resp: reqwest::Response, max: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let b = chunk?;
        if out.len() + b.len() > max {
            bail!("ответ больше {max} байт");
        }
        out.extend_from_slice(&b);
    }
    Ok(out)
}
