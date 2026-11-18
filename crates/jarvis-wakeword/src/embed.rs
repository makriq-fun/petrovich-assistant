//! Streaming-эмбеддинги openWakeWord: 76x32 мел-кадров -> 96 чисел.
//! Свёрточная сеть обрабатывает только новые 8 кадров, сохраняя историю
//! строк между вызовами, поэтому стоимость шага ~1/8 полного окна.

use std::collections::HashMap;

use anyhow::{bail, Result};

use crate::tensors::Tensor;

pub const EMB_WINDOW: usize = 76;
pub const EMB_DIM: usize = 96;
const MAX_ROWS_IN: usize = super::mel::STEP_FRAMES + 2;
const STREAM_PAD: usize = super::mel::STEP_FRAMES - EMB_WINDOW % super::mel::STEP_FRAMES;

struct Conv {
    kh: usize,
    kw: usize,
    ci: usize,
    co: usize,
    /// [kh][kw][ci][co]
    w: Vec<f32>,
    b: Vec<f32>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Conv,
    PoolTime,
    PoolFreq,
}

struct Stage {
    kind: Kind,
    conv: Option<Conv>,
    act: bool,
    w: usize,
    cin: usize,
    nbuf: usize,
    buf: Vec<f32>,
    out: Vec<f32>,
    out_w: usize,
    out_c: usize,
}

pub struct EmbedNet {
    stages: Vec<Stage>,
}

impl EmbedNet {
    pub fn new(t: &HashMap<String, Tensor>) -> Result<Self> {
        let mut convs = Vec::new();
        for i in 0.. {
            let Some(wt) = t.get(&format!("emb.conv{i}.w")) else {
                break;
            };
            let bt = t
                .get(&format!("emb.conv{i}.b"))
                .ok_or_else(|| anyhow::anyhow!("нет emb.conv{i}.b"))?;
            if wt.shape.len() != 4 {
                bail!("emb.conv{i}: форма {:?}", wt.shape);
            }
            let (co, ci, kh, kw) = (wt.shape[0], wt.shape[1], wt.shape[2], wt.shape[3]);
            if bt.data.len() != co {
                bail!("emb.conv{i}: bias {} != {co}", bt.data.len());
            }
            // [co][ci][kh][kw] -> [kh][kw][ci][co]
            let mut w = vec![0.0f32; wt.data.len()];
            for o in 0..co {
                for c in 0..ci {
                    for y in 0..kh {
                        for x in 0..kw {
                            w[((y * kw + x) * ci + c) * co + o] =
                                wt.data[((o * ci + c) * kh + y) * kw + x];
                        }
                    }
                }
            }
            convs.push(Conv {
                kh,
                kw,
                ci,
                co,
                w,
                b: bt.data.clone(),
            });
        }
        if convs.len() != 20 {
            bail!("ожидалось 20 свёрточных слоёв, найдено {}", convs.len());
        }
        let mut stages = Vec::with_capacity(30);
        let mut width = super::mel::MEL_BINS;
        let mut channels = 1usize;
        let add = |kind: Kind, conv: Option<Conv>, act: bool, w: usize, c: usize| {
            let (out_w, out_c) = match kind {
                Kind::Conv => (w, conv.as_ref().map(|v| v.co).unwrap_or(c)),
                Kind::PoolTime | Kind::PoolFreq => (w / 2, c),
            };
            let buf = vec![0.0; MAX_ROWS_IN * w * c];
            let out = vec![0.0; MAX_ROWS_IN * out_w * out_c];
            Stage {
                kind,
                conv,
                act,
                w,
                cin: c,
                nbuf: 0,
                buf,
                out,
                out_w,
                out_c,
            }
        };
        for (i, conv) in convs.into_iter().enumerate() {
            let co = conv.co;
            stages.push(add(Kind::Conv, Some(conv), i < 19, width, channels));
            channels = co;
            match i {
                2 | 10 | 18 => {
                    stages.push(add(Kind::PoolTime, None, false, width, channels));
                    width /= 2;
                }
                6 | 14 => {
                    stages.push(add(Kind::PoolFreq, None, false, width, channels));
                    width /= 2;
                }
                _ => {}
            }
        }
        if width != 1 || channels != EMB_DIM {
            bail!("выход сети {width}x{channels}, нужно 1x{EMB_DIM}");
        }
        let mut net = Self { stages };
        net.reset();
        Ok(net)
    }

    pub fn reset(&mut self) {
        for s in &mut self.stages {
            s.nbuf = 0;
        }
        let pad = vec![0.0f32; STREAM_PAD * super::mel::MEL_BINS];
        let _ = self.push(&pad, STREAM_PAD);
    }

    /// Подаёт `rows` мел-кадров (row-major, 32 значения), возвращает
    /// завершённые эмбеддинги в ряд.
    pub fn push(&mut self, input: &[f32], rows: usize) -> Vec<f32> {
        let mut current = input[..rows * super::mel::MEL_BINS].to_vec();
        let mut cur_rows = rows;
        for idx in 0..self.stages.len() {
            let (kind, w, cin, nbuf) = {
                let s = &self.stages[idx];
                (s.kind, s.w, s.cin, s.nbuf)
            };
            let row = w * cin;
            let out_geometry = {
                let s = &self.stages[idx];
                (s.out_w, s.out_c)
            };
            let stage = &mut self.stages[idx];
            if nbuf + cur_rows > MAX_ROWS_IN {
                // Не должно случаться при STEP_FRAMES=8; защита от порчи модели.
                cur_rows = MAX_ROWS_IN.saturating_sub(nbuf);
            }
            stage.buf[nbuf * row..(nbuf + cur_rows) * row]
                .copy_from_slice(&current[..cur_rows * row]);
            let total = nbuf + cur_rows;
            let (new_rows, keep_rows) = match kind {
                Kind::Conv => {
                    let conv = stage.conv.as_ref().expect("conv");
                    let oh = total.saturating_sub(conv.kh - 1);
                    if oh > 0 {
                        apply_conv(conv, &stage.buf, total, w, &mut stage.out, stage.act);
                    }
                    (oh, conv.kh - 1)
                }
                Kind::PoolTime => {
                    let oh = total / 2;
                    if oh > 0 {
                        max_pool(&stage.buf, total, w, cin, true, &mut stage.out);
                    }
                    (oh, total % 2)
                }
                Kind::PoolFreq => {
                    max_pool(&stage.buf, total, w, cin, false, &mut stage.out);
                    (total, 0)
                }
            };
            let keep_rows = keep_rows.min(total);
            stage
                .buf
                .copy_within((total - keep_rows) * row..total * row, 0);
            stage.nbuf = keep_rows;
            if new_rows == 0 {
                return Vec::new();
            }
            let (ow, oc) = out_geometry;
            let out_len = new_rows * ow * oc;
            current.clear();
            current.extend_from_slice(&stage.out[..out_len]);
            cur_rows = new_rows;
        }
        current
    }
}

fn apply_conv(c: &Conv, input: &[f32], h: usize, w: usize, out: &mut [f32], act: bool) {
    let oh = h - c.kh + 1;
    let out_len = oh * w * c.co;
    for p in (0..out_len).step_by(c.co) {
        out[p..p + c.co].copy_from_slice(&c.b);
    }
    let pad = (c.kw - 1) / 2;
    for ky in 0..c.kh {
        for kx in 0..c.kw {
            let wt = &c.w[(ky * c.kw + kx) * c.ci * c.co..][..c.ci * c.co];
            let x0 = pad.saturating_sub(kx);
            let x1 = (w + pad).saturating_sub(kx).min(w);
            for y in 0..oh {
                for x in x0..x1 {
                    let src = &input[((y + ky) * w + (x + kx - pad)) * c.ci..][..c.ci];
                    let dst = &mut out[(y * w + x) * c.co..][..c.co];
                    for ci in 0..c.ci {
                        let s = src[ci];
                        if s == 0.0 {
                            continue;
                        }
                        let wr = &wt[ci * c.co..][..c.co];
                        for co in 0..c.co {
                            dst[co] += s * wr[co];
                        }
                    }
                }
            }
        }
    }
    if act {
        for v in &mut out[..out_len] {
            if *v < 0.0 {
                *v = (*v * 0.2).max(-0.4);
            }
        }
    }
}

fn max_pool(input: &[f32], h: usize, w: usize, c: usize, pool_time: bool, out: &mut [f32]) {
    let sy = if pool_time { 2 } else { 1 };
    let oh = h / sy;
    let ow = w / 2;
    for y in 0..oh {
        for x in 0..ow {
            let o = &mut out[(y * ow + x) * c..][..c];
            let p00 = &input[((sy * y) * w + 2 * x) * c..][..c];
            let p01 = &input[((sy * y) * w + 2 * x + 1) * c..][..c];
            if !pool_time {
                for i in 0..c {
                    o[i] = p00[i].max(p01[i]);
                }
                continue;
            }
            let p10 = &input[((sy * y + 1) * w + 2 * x) * c..][..c];
            let p11 = &input[((sy * y + 1) * w + 2 * x + 1) * c..][..c];
            for i in 0..c {
                o[i] = p00[i].max(p01[i]).max(p10[i]).max(p11[i]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_wrong_layer_count() {
        let t = HashMap::new();
        assert!(EmbedNet::new(&t).is_err());
    }
}
