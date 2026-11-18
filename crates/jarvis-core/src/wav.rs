//! Минимальный WAV PCM16: без внешних зависимостей, только то, что нужно
//! для STT, локального TTS и тестов.

use crate::errors::{Error, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WavAudio {
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<i16>,
}

impl WavAudio {
    pub fn duration_ms(&self) -> u64 {
        if self.sample_rate == 0 || self.channels == 0 {
            return 0;
        }
        (self.samples.len() as u64 * 1000) / (self.sample_rate as u64 * self.channels as u64)
    }

    /// Среднее по каналам; для моно возвращает копию.
    pub fn to_mono(&self) -> Vec<i16> {
        match self.channels {
            0 | 1 => self.samples.clone(),
            n => self
                .samples
                .chunks_exact(n as usize)
                .map(|frame| {
                    let sum: i32 = frame.iter().map(|s| i32::from(*s)).sum();
                    (sum / i32::from(n)) as i16
                })
                .collect(),
        }
    }
}

/// Кодирует PCM16 в WAV. `samples` — чередующиеся каналы.
pub fn encode(sample_rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
    let data_len = samples.len() * 2;
    let mut out = Vec::with_capacity(44 + data_len);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36u32 + data_len as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * u32::from(channels) * 2).to_le_bytes());
    out.extend_from_slice(&(channels * 2).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

pub fn encode_mono(sample_rate: u32, samples: &[i16]) -> Vec<u8> {
    encode(sample_rate, 1, samples)
}

/// Разбирает WAV с PCM16 (включая WAVE_FORMAT_EXTENSIBLE).
pub fn parse(bytes: &[u8]) -> Result<WavAudio> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(Error::Parse("не WAV/RIFX-файл".into()));
    }
    let mut pos = 12;
    let mut fmt: Option<(u16, u16, u32, u16)> = None; // format, channels, rate, bits
    let mut data: Option<&[u8]> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = pos + 8;
        let end = body
            .checked_add(len)
            .ok_or_else(|| Error::Parse("длина чанка".into()))?;
        if end > bytes.len() {
            return Err(Error::Parse("обрезанный WAV".into()));
        }
        match id {
            b"fmt " => {
                if len < 16 {
                    return Err(Error::Parse("короткий fmt".into()));
                }
                let mut format = u16::from_le_bytes([bytes[body], bytes[body + 1]]);
                let channels = u16::from_le_bytes([bytes[body + 2], bytes[body + 3]]);
                let rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().unwrap());
                let bits = u16::from_le_bytes([bytes[body + 14], bytes[body + 15]]);
                if format == 0xFFFE {
                    if len < 26 {
                        return Err(Error::Parse("короткий extensible fmt".into()));
                    }
                    // Первые два байта SubFormat GUID — код формата.
                    format = u16::from_le_bytes([bytes[body + 24], bytes[body + 25]]);
                }
                fmt = Some((format, channels, rate, bits));
            }
            b"data" => data = Some(&bytes[body..end]),
            _ => {}
        }
        pos = end + (len & 1);
    }
    let (format, channels, rate, bits) = fmt.ok_or_else(|| Error::Parse("в WAV нет fmt".into()))?;
    if format != 1 {
        return Err(Error::Unsupported(format!(
            "WAV формат {format}, поддерживается только PCM"
        )));
    }
    if bits != 16 {
        return Err(Error::Unsupported(format!(
            "WAV {bits} бит, поддерживается только 16"
        )));
    }
    if channels == 0 || rate == 0 {
        return Err(Error::Parse("нулевые каналы или частота".into()));
    }
    let data = data.ok_or_else(|| Error::Parse("в WAV нет data".into()))?;
    let samples: Vec<i16> = data
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();
    Ok(WavAudio {
        sample_rate: rate,
        channels,
        samples,
    })
}

/// Разбирает заголовок, не копируя сэмплы: нужен для потокового TTS.
/// Возвращает (частота, каналы, смещение данных).
pub fn probe(bytes: &[u8]) -> Result<(u32, u16, usize)> {
    let audio = parse_prefix(bytes)?;
    Ok(audio)
}

fn parse_prefix(bytes: &[u8]) -> Result<(u32, u16, usize)> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(Error::Parse("не WAV".into()));
    }
    let mut pos = 12;
    let mut fmt = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = pos + 8;
        if id == b"fmt " && body + 16 <= bytes.len() {
            let format = u16::from_le_bytes([bytes[body], bytes[body + 1]]);
            let channels = u16::from_le_bytes([bytes[body + 2], bytes[body + 3]]);
            let rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().unwrap());
            let bits = u16::from_le_bytes([bytes[body + 14], bytes[body + 15]]);
            if format != 1 || bits != 16 {
                return Err(Error::Unsupported("нужен WAV PCM16".into()));
            }
            fmt = Some((rate, channels));
        }
        if id == b"data" {
            return fmt
                .map(|(rate, ch)| (rate, ch, body))
                .ok_or_else(|| Error::Parse("data раньше fmt".into()));
        }
        pos = body + len + (len & 1);
    }
    Err(Error::Parse("в WAV нет data".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_mono_and_stereo() {
        let mono: Vec<i16> = (-1000..1000).collect();
        let wav = encode(16_000, 1, &mono);
        let parsed = parse(&wav).unwrap();
        assert_eq!(parsed.sample_rate, 16_000);
        assert_eq!(parsed.channels, 1);
        assert_eq!(parsed.samples, mono);

        let stereo: Vec<i16> = (0..400).map(|i| (i * 7) as i16).collect();
        let parsed = parse(&encode(24_000, 2, &stereo)).unwrap();
        assert_eq!(parsed.channels, 2);
        assert_eq!(parsed.samples, stereo);
        assert_eq!(parsed.to_mono().len(), 200);
    }

    #[test]
    fn rejects_float_wav() {
        let mut wav = encode(16_000, 1, &[0; 10]);
        wav[20] = 3; // IEEE float
        assert!(parse(&wav).is_err());
    }

    #[test]
    fn probe_finds_data_without_copying() {
        let wav = encode(16_000, 1, &[1, 2, 3, 4]);
        let (rate, channels, offset) = probe(&wav).unwrap();
        assert_eq!((rate, channels, offset), (16_000, 1, 44));
    }

    #[test]
    fn truncated_is_rejected() {
        let wav = encode(16_000, 1, &[0; 100]);
        assert!(parse(&wav[..wav.len() - 10]).is_err());
    }
}
