//! Контейнер OWW1: плоские float32-тензоры feature-backbone openWakeWord.
//! Формат совместим с обученными моделями станции; сами веса в репозиторий
//! не входят, кроме открытого backbone (см. THIRD_PARTY_NOTICES.md).

use std::collections::HashMap;

use anyhow::{bail, Context, Result};

#[derive(Clone, Debug)]
pub struct Tensor {
    pub shape: Vec<usize>,
    pub data: Vec<f32>,
}

pub fn read(bytes: &[u8]) -> Result<HashMap<String, Tensor>> {
    if bytes.len() < 8 || &bytes[..4] != b"OWW1" {
        bail!("OWW1: неверная сигнатура");
    }
    let n = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let mut p = 8usize;
    let mut out = HashMap::with_capacity(n);
    let need = |p: usize, k: usize| -> Result<()> {
        if p + k > bytes.len() {
            bail!("OWW1: обрезанный файл");
        }
        Ok(())
    };
    for _ in 0..n {
        need(p, 2)?;
        let nl = u16::from_le_bytes([bytes[p], bytes[p + 1]]) as usize;
        p += 2;
        need(p, nl + 1)?;
        let name = String::from_utf8_lossy(&bytes[p..p + nl]).into_owned();
        p += nl;
        let nd = bytes[p] as usize;
        p += 1;
        need(p, 4 * nd)?;
        let mut shape = Vec::with_capacity(nd);
        let mut size = 1usize;
        for _ in 0..nd {
            let d = u32::from_le_bytes(bytes[p..p + 4].try_into().unwrap()) as usize;
            shape.push(d);
            size = size.checked_mul(d).context("OWW1: переполнение размера")?;
            p += 4;
        }
        need(p, 4 * size)?;
        let mut data = Vec::with_capacity(size);
        for _ in 0..size {
            data.push(f32::from_le_bytes(bytes[p..p + 4].try_into().unwrap()));
            p += 4;
        }
        out.insert(name, Tensor { shape, data });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_tensor(name: &str, shape: &[usize], data: &[f32]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"OWW1");
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&(name.len() as u16).to_le_bytes());
        b.extend_from_slice(name.as_bytes());
        b.push(shape.len() as u8);
        for d in shape {
            b.extend_from_slice(&(*d as u32).to_le_bytes());
        }
        for v in data {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b
    }

    #[test]
    fn reads_tensor() {
        let b = one_tensor("x", &[2, 2], &[1.0, 2.0, 3.0, 4.0]);
        let t = read(&b).unwrap();
        assert_eq!(t["x"].shape, [2, 2]);
        assert_eq!(t["x"].data, [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn rejects_truncated() {
        let b = one_tensor("x", &[2], &[1.0, 2.0]);
        assert!(read(&b[..b.len() - 3]).is_err());
        assert!(read(b"nope").is_err());
    }
}
