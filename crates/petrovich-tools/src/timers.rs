//! Таймеры: отдельная задача на каждый, отмена через oneshot, уведомление
//! демона для озвучки и индикации.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use petrovich_core::events::{TimerEvent, UiEvent};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::Instant;

#[derive(Clone, Debug)]
pub struct TimerFired {
    pub id: u64,
    pub label: String,
}

#[derive(Clone, Debug)]
pub struct TimerInfo {
    pub id: u64,
    pub label: String,
    pub remaining_s: u64,
}

struct Entry {
    label: String,
    deadline: Instant,
    cancel: Option<oneshot::Sender<()>>,
}

#[derive(Clone)]
pub struct TimerManager {
    inner: Arc<Mutex<HashMap<u64, Entry>>>,
    next: Arc<AtomicU64>,
    ui: mpsc::UnboundedSender<UiEvent>,
    announce: mpsc::Sender<TimerFired>,
}

impl TimerManager {
    pub fn new(ui: mpsc::UnboundedSender<UiEvent>, announce: mpsc::Sender<TimerFired>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            next: Arc::new(AtomicU64::new(1)),
            ui,
            announce,
        }
    }

    pub async fn add(&self, seconds: u64, label: String) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        let deadline = Instant::now() + Duration::from_secs(seconds);
        self.inner.lock().await.insert(
            id,
            Entry {
                label: label.clone(),
                deadline,
                cancel: Some(tx),
            },
        );
        let _ = self.ui.send(UiEvent::Timer(TimerEvent::Started {
            id,
            seconds,
            label: label.clone(),
        }));
        let inner = Arc::clone(&self.inner);
        let ui = self.ui.clone();
        let announce = self.announce.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {}
                _ = rx => { return; }
            }
            let removed = inner.lock().await.remove(&id);
            if removed.is_none() {
                return;
            }
            let _ = ui.send(UiEvent::Timer(TimerEvent::Fired {
                id,
                label: label.clone(),
            }));
            let _ = announce.send(TimerFired { id, label }).await;
        });
        id
    }

    pub async fn cancel(&self, id: u64) -> bool {
        let mut map = self.inner.lock().await;
        if let Some(mut e) = map.remove(&id) {
            if let Some(tx) = e.cancel.take() {
                let _ = tx.send(());
            }
            let _ = self.ui.send(UiEvent::Timer(TimerEvent::Cancelled { id }));
            true
        } else {
            false
        }
    }

    pub async fn list(&self) -> Vec<TimerInfo> {
        let now = Instant::now();
        let map = self.inner.lock().await;
        let mut out: Vec<TimerInfo> = map
            .iter()
            .map(|(id, e)| TimerInfo {
                id: *id,
                label: e.label.clone(),
                remaining_s: e.deadline.saturating_duration_since(now).as_secs(),
            })
            .collect();
        out.sort_by_key(|t| t.remaining_s);
        out
    }

    pub async fn count(&self) -> usize {
        self.inner.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fires_and_notifies() {
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let (ann_tx, mut ann_rx) = mpsc::channel(4);
        let m = TimerManager::new(ui_tx, ann_tx);
        let id = m.add(1, "чай".into()).await;
        let fired = tokio::time::timeout(Duration::from_secs(3), ann_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fired.id, id);
        assert_eq!(fired.label, "чай");
        // Первое событие — запуск, второе — срабатывание.
        let _ = ui_rx.recv().await;
        assert!(matches!(
            ui_rx.recv().await,
            Some(UiEvent::Timer(TimerEvent::Fired { .. }))
        ));
        assert_eq!(m.count().await, 0);
    }

    #[tokio::test]
    async fn cancel_prevents_fire() {
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let (ann_tx, mut ann_rx) = mpsc::channel(4);
        let m = TimerManager::new(ui_tx, ann_tx);
        let id = m.add(1, "x".into()).await;
        assert!(m.cancel(id).await);
        assert!(!m.cancel(id).await);
        assert!(
            tokio::time::timeout(Duration::from_millis(1200), ann_rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn lists_remaining() {
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let (ann_tx, _ann_rx) = mpsc::channel(4);
        let m = TimerManager::new(ui_tx, ann_tx);
        m.add(10, "a".into()).await;
        m.add(20, "b".into()).await;
        let l = m.list().await;
        assert_eq!(l.len(), 2);
        assert!(l[0].remaining_s <= 10);
        assert!(l[1].remaining_s > l[0].remaining_s);
    }
}
