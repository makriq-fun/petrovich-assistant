//! Потоки захвата/воспроизведения и покадровая обработка входа.
//!
//! Захват: ALSA `default`, 48 кГц, 6 каналов. Если VQE прошивки доступен,
//! он получает 4 микрофона и 2 аппаратных референса и отдаёт 16 кГц «omni».
//! Иначе микрофоны смешиваются, прореживаются и проходят собственный NLMS.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use petrovich_core::config::{AudioConfig, VqeMode};
use petrovich_core::profile::HardwareProfile;
use tokio::sync::{mpsc, oneshot};

use crate::aec::{DelayLine, Nlms};
use crate::alsa::Pcm;
use crate::denoise::NoiseGate;
use crate::resample::{Decimator, LinearResampler};
use crate::vqe::{Vqe, VQE_OMNI};

pub struct CaptureBlock {
    pub samples: Vec<i16>,
    pub frames: usize,
    pub channels: usize,
    pub rate: u32,
}

pub struct PcmFrame {
    pub samples: Vec<i16>,
    /// Аппаратный референс на той же частоте (для диагностики/AEC).
    pub reference: Vec<i16>,
}

pub enum PlaybackMsg {
    Pcm {
        generation: u64,
        samples: Vec<i16>,
        rate: u32,
    },
    Stop {
        generation: u64,
    },
    Drain {
        generation: u64,
        done: oneshot::Sender<()>,
    },
}

/// Разделяемый дескриптор вывода. Поколение отсекает старые буферы после
/// прерывания: они не должны звучать после новой команды.
#[derive(Clone)]
pub struct PlaybackHandle {
    tx: mpsc::UnboundedSender<PlaybackMsg>,
    generation: Arc<AtomicU64>,
    playing: Arc<AtomicBool>,
}

impl PlaybackHandle {
    /// Отсоединённый вывод для тестов и диагностики: звук никуда не идёт.
    pub fn detached() -> Self {
        let (tx, _rx) = mpsc::unbounded_channel();
        Self {
            tx,
            generation: Arc::new(AtomicU64::new(0)),
            playing: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Ставит PCM в очередь; возвращает estimated длительность звучания.
    pub fn play_pcm(&self, samples: Vec<i16>, rate: u32) -> std::time::Duration {
        if samples.is_empty() || rate == 0 {
            return std::time::Duration::ZERO;
        }
        let duration = std::time::Duration::from_secs_f64(samples.len() as f64 / f64::from(rate));
        let generation = self.generation.load(Ordering::Acquire);
        self.playing.store(true, Ordering::Release);
        let _ = self.tx.send(PlaybackMsg::Pcm {
            generation,
            samples,
            rate,
        });
        duration
    }

    pub fn play_wav(&self, bytes: &[u8]) -> Result<std::time::Duration> {
        let wav = petrovich_core::wav::parse(bytes)?;
        Ok(self.play_pcm(wav.samples, wav.sample_rate))
    }

    /// Немедленно прерывает звук; всё, что уже в очереди, будет отброшено.
    pub fn stop(&self) {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.playing.store(false, Ordering::Release);
        let _ = self.tx.send(PlaybackMsg::Stop { generation });
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Acquire)
    }

    /// Ждёт, пока очередь и ALSA-буфер действительно доиграют.
    pub async fn wait_idle(&self) {
        let (done, wait) = oneshot::channel();
        let generation = self.generation.load(Ordering::Acquire);
        if self
            .tx
            .send(PlaybackMsg::Drain { generation, done })
            .is_err()
        {
            return;
        }
        let _ = wait.await;
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
}

pub struct AudioEngine {
    pub capture: mpsc::Receiver<CaptureBlock>,
    pub playback: PlaybackHandle,
    pub volume: Arc<crate::volume::AmixerVolume>,
}

impl AudioEngine {
    pub fn start(
        cfg: &AudioConfig,
        profile: &'static HardwareProfile,
        volume_step: i32,
    ) -> Result<Self> {
        crate::alsa::cleanup_stale_dmix();
        let capture = Pcm::open(
            Some(&cfg.alsa_lib),
            &cfg.capture_pcm,
            cfg.capture_rate,
            cfg.capture_channels,
            cfg.period_ms,
            true,
        )
        .with_context(|| format!("захват {} @ {} Гц", cfg.capture_pcm, cfg.capture_rate))?;
        let playback = Pcm::open(
            Some(&cfg.alsa_lib),
            &cfg.playback_pcm,
            cfg.playback_rate,
            2,
            cfg.period_ms,
            false,
        )
        .with_context(|| format!("вывод {} @ {} Гц", cfg.playback_pcm, cfg.playback_rate))?;

        let (capture_tx, capture_rx) = mpsc::channel::<CaptureBlock>(8);
        let capture_channels = cfg.capture_channels as usize;
        let capture_rate = cfg.capture_rate;
        let mut capture_pcm = capture;
        thread::Builder::new()
            .name("petrovich-capture".into())
            .spawn(move || {
                let frames = capture_pcm.frames_per_period();
                let mut buf = vec![0i16; frames * capture_channels];
                loop {
                    match capture_pcm.read(&mut buf) {
                        Ok(0) => thread::sleep(Duration::from_millis(10)),
                        Ok(n) => {
                            let samples = buf[..n * capture_channels].to_vec();
                            if capture_tx
                                .blocking_send(CaptureBlock {
                                    samples,
                                    frames: n,
                                    channels: capture_channels,
                                    rate: capture_rate,
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("захват ALSA: {e:#}; повтор через 200 мс");
                            thread::sleep(Duration::from_millis(200));
                        }
                    }
                }
            })
            .context("поток захвата")?;

        let (playback_tx, mut playback_rx) = mpsc::unbounded_channel::<PlaybackMsg>();
        let generation = Arc::new(AtomicU64::new(0));
        let playing = Arc::new(AtomicBool::new(false));
        let playing_thread = Arc::clone(&playing);
        let gen_thread = Arc::clone(&generation);
        let playback_rate = cfg.playback_rate;
        let mut playback_pcm = playback;
        thread::Builder::new()
            .name("petrovich-playback".into())
            .spawn(move || {
                let mut resampler: Option<(u32, LinearResampler)> = None;
                while let Some(msg) = playback_rx.blocking_recv() {
                    match msg {
                        PlaybackMsg::Stop { .. } => {
                            playback_pcm.stop();
                            playing_thread.store(false, Ordering::Release);
                            resampler = None;
                        }
                        PlaybackMsg::Drain {
                            generation: g,
                            done,
                        } => {
                            if g == gen_thread.load(Ordering::Acquire) {
                                if let Err(e) = playback_pcm.drain() {
                                    tracing::warn!("вывод ALSA: {e:#}");
                                }
                                playing_thread.store(false, Ordering::Release);
                            }
                            let _ = done.send(());
                        }
                        PlaybackMsg::Pcm {
                            generation: g,
                            samples,
                            rate,
                        } => {
                            if g != gen_thread.load(Ordering::Acquire) {
                                continue;
                            }
                            let mono = samples;
                            let mono = if rate != playback_rate {
                                if resampler.as_ref().map(|(r, _)| *r) != Some(rate) {
                                    resampler =
                                        Some((rate, LinearResampler::new(rate, playback_rate)));
                                }
                                let (_, r) = resampler.as_mut().expect("resampler");
                                r.process(&mono)
                            } else {
                                resampler = None;
                                mono
                            };
                            let mut stereo = Vec::with_capacity(mono.len() * 2);
                            for s in mono {
                                stereo.push(s);
                                stereo.push(s);
                            }
                            if let Err(e) = playback_pcm.write(&stereo) {
                                tracing::warn!("вывод ALSA: {e:#}");
                                thread::sleep(Duration::from_millis(100));
                                continue;
                            }
                            if g != gen_thread.load(Ordering::Acquire) {
                                playback_pcm.stop();
                                playing_thread.store(false, Ordering::Release);
                            }
                        }
                    }
                }
            })
            .context("поток вывода")?;

        let volume = Arc::new(crate::volume::AmixerVolume::new(
            cfg.playback_pcm.clone(),
            volume_step,
        ));
        let _ = profile;
        Ok(Self {
            capture: capture_rx,
            playback: PlaybackHandle {
                tx: playback_tx,
                generation,
                playing,
            },
            volume,
        })
    }
}

/// Покадровая обработка: VQE прошивки или fallback DSP.
pub struct AudioFrontend {
    cfg: AudioConfig,
    vqe: Option<Vqe>,
    pending_mics: Vec<f32>,
    pending_spk: Vec<f32>,
    pending_frames: usize,
    mic_dec: Decimator,
    ref_dec: Decimator,
    aec: Option<Nlms>,
    denoise: Option<NoiseGate>,
    delay: DelayLine,
}

impl AudioFrontend {
    pub fn new(cfg: &AudioConfig) -> Result<Self> {
        if cfg.downmix_before_vqe {
            tracing::warn!("downmix_before_vqe игнорируется: VQE требует отдельные каналы");
        }
        let vqe = match cfg.vqe {
            VqeMode::Off => None,
            VqeMode::Auto | VqeMode::Required => {
                match Vqe::new(
                    &cfg.vqe_preset,
                    &cfg.vqe_models,
                    cfg.mic_channels.len(),
                    cfg.reference_channels.len(),
                ) {
                    Ok(v) => {
                        tracing::info!(
                            "VQE '{}': {} входных кадров -> {} выходных",
                            cfg.vqe_preset,
                            v.chunk(),
                            v.frame()
                        );
                        Some(v)
                    }
                    Err(e) if cfg.vqe == VqeMode::Required => {
                        return Err(e).context("VQE обязателен по конфигурации");
                    }
                    Err(e) => {
                        tracing::warn!("VQE недоступен ({e:#}); собственный DSP");
                        None
                    }
                }
            }
        };
        if vqe.is_none() && cfg.aec && cfg.reference_channels.is_empty() {
            tracing::warn!("нет каналов референса: эхоподавление выключено");
        }
        let delay_samples =
            (u64::from(cfg.aec_delay_ms) * u64::from(cfg.sample_rate) / 1000) as usize;
        Ok(Self {
            cfg: cfg.clone(),
            vqe,
            pending_mics: Vec::new(),
            pending_spk: Vec::new(),
            pending_frames: 0,
            mic_dec: Decimator::new(cfg.capture_rate, cfg.sample_rate),
            ref_dec: Decimator::new(cfg.capture_rate, cfg.sample_rate),
            aec: (cfg.aec && !cfg.reference_channels.is_empty()).then(|| Nlms::new(256, 0.4)),
            denoise: cfg.denoise.then(|| NoiseGate::new(cfg.sample_rate, 9.0)),
            delay: DelayLine::new(delay_samples),
        })
    }

    pub fn uses_vqe(&self) -> bool {
        self.vqe.is_some()
    }

    pub fn push(&mut self, block: &CaptureBlock) -> Vec<PcmFrame> {
        if block.channels != self.cfg.capture_channels as usize
            || block.rate != self.cfg.capture_rate
        {
            tracing::warn!(
                "блок {} каналов @ {} Гц не совпадает с конфигурацией",
                block.channels,
                block.rate
            );
            return Vec::new();
        }
        if self.vqe.is_some() {
            self.push_vqe(block)
        } else {
            self.push_fallback(block)
        }
    }

    fn push_vqe(&mut self, block: &CaptureBlock) -> Vec<PcmFrame> {
        let channels = block.channels;
        for frame in block.samples.chunks_exact(channels) {
            for &ch in &self.cfg.mic_channels {
                self.pending_mics
                    .push(f32::from(frame[ch as usize]) / 32768.0);
            }
            for &ch in &self.cfg.reference_channels {
                self.pending_spk
                    .push(f32::from(frame[ch as usize]) / 32768.0);
            }
            self.pending_frames += 1;
        }
        let Some(vqe) = self.vqe.as_mut() else {
            return Vec::new();
        };
        let chunk = vqe.chunk();
        let mic_ch = self.cfg.mic_channels.len();
        let spk_ch = self.cfg.reference_channels.len();
        let mut out = Vec::new();
        while self.pending_frames >= chunk {
            let mic_len = chunk * mic_ch;
            let spk_len = chunk * spk_ch;
            let mics: Vec<f32> = self.pending_mics.drain(..mic_len).collect();
            let spk: Vec<f32> = self.pending_spk.drain(..spk_len).collect();
            self.pending_frames -= chunk;
            if let Err(e) = vqe.process(&mics, &spk) {
                tracing::warn!("VQE: {e:#}; переключаюсь на собственный DSP");
                self.vqe = None;
                self.pending_mics.clear();
                self.pending_spk.clear();
                self.pending_frames = 0;
                return out;
            }
            let omni = match vqe.output(VQE_OMNI) {
                Ok(v) if !v.is_empty() => v,
                Ok(_) => {
                    tracing::warn!("VQE не отдал канал omni; собственный DSP");
                    self.vqe = None;
                    return out;
                }
                Err(e) => {
                    tracing::warn!("VQE output: {e:#}; собственный DSP");
                    self.vqe = None;
                    return out;
                }
            };
            let samples: Vec<i16> = omni
                .iter()
                .map(|v| (v.clamp(-1.0, 1.0) * 32767.0).round() as i16)
                .collect();
            out.push(PcmFrame {
                samples,
                reference: Vec::new(),
            });
        }
        out
    }

    fn push_fallback(&mut self, block: &CaptureBlock) -> Vec<PcmFrame> {
        let channels = block.channels;
        let n = block.frames;
        let mut mic48 = Vec::with_capacity(n);
        let mut ref48 = Vec::with_capacity(n);
        for frame in block.samples.chunks_exact(channels) {
            let m: i32 = self
                .cfg
                .mic_channels
                .iter()
                .map(|&c| i32::from(frame[c as usize]))
                .sum();
            mic48.push((m / self.cfg.mic_channels.len() as i32) as i16);
            if !self.cfg.reference_channels.is_empty() {
                let r: i32 = self
                    .cfg
                    .reference_channels
                    .iter()
                    .map(|&c| i32::from(frame[c as usize]))
                    .sum();
                ref48.push((r / self.cfg.reference_channels.len() as i32) as i16);
            }
        }
        let mut mic = self.mic_dec.process(&mic48);
        let reference = if ref48.is_empty() {
            Vec::new()
        } else {
            self.ref_dec.process(&ref48)
        };
        let samples = if let Some(aec) = self.aec.as_mut() {
            if reference.len() == mic.len() {
                let mut clean = Vec::with_capacity(mic.len());
                for (&m, &r) in mic.iter().zip(&reference) {
                    let r = self.delay.push(f32::from(r) / 32768.0);
                    let y = aec.process(f32::from(m) / 32768.0, r);
                    clean.push((y.clamp(-1.0, 1.0) * 32767.0).round() as i16);
                }
                mic = clean;
            }
            mic
        } else {
            mic
        };
        let samples = match self.denoise.as_mut() {
            Some(g) => g.process(&samples),
            None => samples,
        };
        vec![PcmFrame { samples, reference }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AudioConfig {
        AudioConfig::default()
    }

    fn block(tone_hz: f32, echo: bool, frames: usize) -> CaptureBlock {
        let mut samples = Vec::with_capacity(frames * 6);
        for i in 0..frames {
            let v = (2.0 * std::f32::consts::PI * tone_hz * i as f32 / 48_000.0).sin() * 8000.0;
            let v = v as i16;
            let e = if echo { v / 3 } else { 0 };
            samples.extend_from_slice(&[v, v, v, v, e, e]);
        }
        CaptureBlock {
            samples,
            frames,
            channels: 6,
            rate: 48_000,
        }
    }

    #[test]
    fn fallback_downmixes_and_resamples() {
        let c = cfg();
        let mut f = AudioFrontend::new(&c).unwrap();
        assert!(!f.uses_vqe());
        let out = f.push(&block(440.0, false, 960));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].samples.len(), 320);
        assert_eq!(out[0].reference.len(), 320);
    }

    fn echo_only_block(frames: usize) -> CaptureBlock {
        let mut samples = Vec::with_capacity(frames * 6);
        for i in 0..frames {
            let v = (2.0 * std::f32::consts::PI * 300.0 * i as f32 / 48_000.0).sin() * 6000.0;
            let e = v as i16;
            samples.extend_from_slice(&[0, 0, 0, 0, e, e]);
        }
        CaptureBlock {
            samples,
            frames,
            channels: 6,
            rate: 48_000,
        }
    }

    #[test]
    fn fallback_aec_reduces_echo() {
        let mut f = AudioFrontend::new(&cfg()).unwrap();
        for _ in 0..20 {
            let _ = f.push(&echo_only_block(960));
        }
        let out = f.push(&echo_only_block(960));
        let clean = &out[0].samples;
        let raw = &out[0].reference;
        let rms = |x: &[i16]| {
            (x.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
        };
        assert!(rms(clean) < rms(raw) * 0.5, "{} / {}", rms(clean), rms(raw));
    }

    #[test]
    fn mismatched_block_is_ignored() {
        let mut f = AudioFrontend::new(&cfg()).unwrap();
        let b = CaptureBlock {
            samples: vec![0; 10],
            frames: 5,
            channels: 2,
            rate: 16_000,
        };
        assert!(f.push(&b).is_empty());
    }
}
