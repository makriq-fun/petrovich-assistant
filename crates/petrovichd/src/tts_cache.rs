//! Небольшой кэш готовых коротких фраз: таймеры и будильники звучат
//! повторяющимися словами, их незачем синтезировать заново.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::stream::{self, StreamExt};
use petrovich_core::config::TtsFormat;
use petrovich_providers::tts::AudioStream;
use tokio::sync::Mutex;

const DEFAULT_MAX: usize = 32;

#[derive(Clone)]
pub struct CachedAudio {
    pub format: TtsFormat,
    pub sample_rate: u32,
    pub channels: u16,
    pub bytes: Bytes,
}

impl CachedAudio {
    pub fn to_stream(&self) -> AudioStream {
        let bytes = self.bytes.clone();
        AudioStream {
            format: self.format,
            sample_rate: self.sample_rate,
            channels: self.channels,
            stream: stream::once(async move { Ok(bytes) }).boxed(),
        }
    }
}

#[derive(Clone)]
pub struct TtsCache {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    map: HashMap<String, CachedAudio>,
    max: usize,
}

impl Default for TtsCache {
    fn default() -> Self {
        Self::new(DEFAULT_MAX)
    }
}

impl TtsCache {
    pub fn new(max: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                map: HashMap::new(),
                max,
            })),
        }
    }

    pub async fn get(&self, text: &str) -> Option<CachedAudio> {
        let key = text.trim();
        if key.is_empty() {
            return None;
        }
        self.inner.lock().await.map.get(key).cloned()
    }

    pub async fn put(&self, text: &str, audio: CachedAudio) {
        let key = text.trim().to_owned();
        if key.is_empty() || audio.bytes.is_empty() {
            return;
        }
        let mut inner = self.inner.lock().await;
        if inner.max == 0 {
            return;
        }
        if inner.map.len() >= inner.max && !inner.map.contains_key(&key) {
            inner.map.clear();
        }
        inner.map.insert(key, audio);
    }

    #[cfg(test)]
    pub async fn len(&self) -> usize {
        self.inner.lock().await.map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(bytes: &[u8]) -> CachedAudio {
        CachedAudio {
            format: TtsFormat::Pcm,
            sample_rate: 24_000,
            channels: 1,
            bytes: Bytes::copy_from_slice(bytes),
        }
    }

    #[tokio::test]
    async fn stores_and_replays_phrase() {
        let c = TtsCache::new(2);
        c.put("Таймер сработал.", audio(b"abc")).await;
        let hit = c.get(" Таймер сработал. ").await.unwrap();
        assert_eq!(hit.bytes.as_ref(), b"abc");
        assert_eq!(hit.sample_rate, 24_000);
        let mut s = hit.to_stream();
        assert_eq!(s.stream.next().await.unwrap().unwrap().as_ref(), b"abc");
    }

    #[tokio::test]
    async fn bounded_cache_drops_old_phrases() {
        let c = TtsCache::new(2);
        c.put("a", audio(b"1")).await;
        c.put("b", audio(b"2")).await;
        c.put("c", audio(b"3")).await;
        assert_eq!(c.len().await, 1);
        assert!(c.get("a").await.is_none());
        assert!(c.get("c").await.is_some());
    }
}
