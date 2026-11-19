//! microWakeWord v2 (TFLite int8, streaming) + TFLM microfrontend.
//!
//! Модель — пара JSON-манифест + .tflite из microwakeword-models. Фронтенд
//! TFLM компилируется из Apache-2.0 исходников; TFLite C API загружается из
//! прошивки через dlopen.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use libloading::Library;
use serde::Deserialize;

use crate::detector::Detection;

const TFLITE_VENDOR: &str = "/system/vendor/quasar/libs/libtflite.so";
const TFLITE_PRIVATE: &str = "libtensorflowlite_c.so";

/// Сначала своя TFLite рядом с бинарником, затем библиотека прошивки.
/// Переменная `SKYNET_TFLITE` нужна только для диагностики.
fn library_path() -> PathBuf {
    if let Some(p) = std::env::var_os("SKYNET_TFLITE").map(PathBuf::from) {
        if p.is_file() {
            return p;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for p in [dir.join(TFLITE_PRIVATE), dir.join("lib").join(TFLITE_PRIVATE)] {
                if p.is_file() {
                    return p;
                }
            }
        }
    }
    PathBuf::from(TFLITE_VENDOR)
}

#[repr(C)]
struct MfHandle {
    _private: [u8; 0],
}

extern "C" {
    fn mf_create() -> *mut MfHandle;
    fn mf_process(h: *mut MfHandle, samples: *const i16, count: usize, out: *mut i8) -> c_int;
    fn mf_reset(h: *mut MfHandle);
    fn mf_free(h: *mut MfHandle);
}

type TfModel = c_void;
type TfInterpreter = c_void;
type TfTensor = c_void;

struct Tflite {
    _lib: Library,
    model_create_file: unsafe extern "C" fn(*const c_char) -> *mut TfModel,
    version: unsafe extern "C" fn() -> *const c_char,
    model_delete: unsafe extern "C" fn(*mut TfModel),
    options_create: unsafe extern "C" fn() -> *mut c_void,
    options_delete: unsafe extern "C" fn(*mut c_void),
    interp_create: unsafe extern "C" fn(*mut TfModel, *const c_void) -> *mut TfInterpreter,
    interp_delete: unsafe extern "C" fn(*mut TfInterpreter),
    allocate: unsafe extern "C" fn(*mut TfInterpreter) -> c_int,
    input: unsafe extern "C" fn(*mut TfInterpreter, c_int) -> *mut TfTensor,
    output: unsafe extern "C" fn(*mut TfInterpreter, c_int) -> *mut TfTensor,
    copy_in: unsafe extern "C" fn(*mut TfTensor, *const c_void, usize) -> c_int,
    copy_out: unsafe extern "C" fn(*mut TfTensor, *mut c_void, usize) -> c_int,
    tensor_bytes: unsafe extern "C" fn(*const TfTensor) -> usize,
    invoke: unsafe extern "C" fn(*mut TfInterpreter) -> c_int,
}

fn parse_major_minor(text: &str) -> Option<(u32, u32)> {
    let text = text.trim_start_matches(|c: char| !c.is_ascii_digit());
    let mut parts = text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts
        .next()?
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>();
    Some((major, minor.parse().ok()?))
}

/// Ищет `min_runtime_version` в flatbuffer модели. Формат TFLite хранит его
/// в metadata; для защиты от падения старого `libtflite.so` достаточно
/// выделить semver из хвоста после имени ключа.
fn model_min_runtime(bytes: &[u8]) -> Option<(u32, u32)> {
    let key = b"min_runtime_version";
    let pos = bytes.windows(key.len()).position(|w| w == key)?;
    let tail = &bytes[pos + key.len()..];
    let mut i = 0;
    while i < tail.len() {
        if !tail[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < tail.len() && (tail[i].is_ascii_digit() || tail[i] == b'.') {
            i += 1;
        }
        let text = std::str::from_utf8(&tail[start..i]).ok()?;
        if let Some(v) = parse_major_minor(text) {
            if v.0 < 100 && v.1 < 100 {
                return Some(v);
            }
        }
    }
    None
}

impl Tflite {
    fn runtime_version(&self) -> Option<(u32, u32)> {
        // SAFETY: TfLiteVersion возвращает статическую строку.
        let ptr = unsafe { (self.version)() };
        if ptr.is_null() {
            return None;
        }
        let text = unsafe { CStr::from_ptr(ptr) }.to_str().ok()?;
        parse_major_minor(text)
    }

    fn load() -> Result<Self> {
        let path = library_path();
        // SAFETY: библиотека проверяется по символам ниже.
        let lib = unsafe { Library::new(&path) }
            .with_context(|| format!("dlopen {}", path.display()))?;
        unsafe fn sym<T: Copy>(lib: &Library, name: &[u8]) -> Result<T> {
            // SAFETY: T — точный тип символа C API.
            unsafe { lib.get::<T>(name) }
                .map(|s| *s)
                .map_err(|e| anyhow::anyhow!("{}: {e}", String::from_utf8_lossy(name)))
        }
        // SAFETY: сигнатуры TFLite C API.
        unsafe {
            Ok(Self {
                model_create_file: sym(&lib, b"TfLiteModelCreateFromFile\0")?,
                version: sym(&lib, b"TfLiteVersion\0")?,
                model_delete: sym(&lib, b"TfLiteModelDelete\0")?,
                options_create: sym(&lib, b"TfLiteInterpreterOptionsCreate\0")?,
                options_delete: sym(&lib, b"TfLiteInterpreterOptionsDelete\0")?,
                interp_create: sym(&lib, b"TfLiteInterpreterCreate\0")?,
                interp_delete: sym(&lib, b"TfLiteInterpreterDelete\0")?,
                allocate: sym(&lib, b"TfLiteInterpreterAllocateTensors\0")?,
                input: sym(&lib, b"TfLiteInterpreterGetInputTensor\0")?,
                output: sym(&lib, b"TfLiteInterpreterGetOutputTensor\0")?,
                copy_in: sym(&lib, b"TfLiteTensorCopyFromBuffer\0")?,
                copy_out: sym(&lib, b"TfLiteTensorCopyToBuffer\0")?,
                tensor_bytes: sym(&lib, b"TfLiteTensorByteSize\0")?,
                invoke: sym(&lib, b"TfLiteInterpreterInvoke\0")?,
                _lib: lib,
            })
        }
    }
}

/// Стандартная голова openWakeWord: вход `[1,16,96]` float32,
/// выход `[1,1]` float32. Эмбеддинги считает `Features` (OWW1).
pub struct TfliteHead {
    tflite: Tflite,
    model: *mut TfModel,
    interp: *mut TfInterpreter,
    input: *mut TfTensor,
    output: *mut TfTensor,
}

// SAFETY: экземпляр используется одним потоком; C-указатели не разделяются.
unsafe impl Send for TfliteHead {}

impl TfliteHead {
    pub fn load(path: &Path) -> Result<Self> {
        let tflite = Tflite::load()?;
        // SAFETY: валидный путь; модель живёт, пока жив TfliteHead.
        let (model, interp) = unsafe {
            let cpath = CString::new(path.to_string_lossy().as_bytes())
                .with_context(|| path.display().to_string())?;
            let model = (tflite.model_create_file)(cpath.as_ptr());
            if model.is_null() {
                bail!("TfLiteModelCreateFromFile: {path:?}");
            }
            let options = (tflite.options_create)();
            let interp = (tflite.interp_create)(model, options);
            (tflite.options_delete)(options);
            if interp.is_null() {
                (tflite.model_delete)(model);
                bail!("TfLiteInterpreterCreate");
            }
            if (tflite.allocate)(interp) != 0 {
                (tflite.interp_delete)(interp);
                (tflite.model_delete)(model);
                bail!("TfLiteInterpreterAllocateTensors");
            }
            (model, interp)
        };
        let input = unsafe { (tflite.input)(interp, 0) };
        let output = unsafe { (tflite.output)(interp, 0) };
        if input.is_null() || output.is_null() {
            unsafe {
                (tflite.interp_delete)(interp);
                (tflite.model_delete)(model);
            }
            bail!("TFLite: нет входного/выходного тензора");
        }
        let in_bytes = unsafe { (tflite.tensor_bytes)(input) };
        let out_bytes = unsafe { (tflite.tensor_bytes)(output) };
        if in_bytes != 16 * 96 * 4 || out_bytes != 4 {
            // SAFETY: тензоры принадлежат interp.
            unsafe {
                (tflite.interp_delete)(interp);
                (tflite.model_delete)(model);
            }
            bail!(
                "TFLite: вход {in_bytes} байт (нужно {}), выход {out_bytes} байт",
                16 * 96 * 4
            );
        }
        Ok(Self {
            tflite,
            model,
            interp,
            input,
            output,
        })
    }

    /// `x` — 16 эмбеддингов по 96, старые первыми.
    pub fn score(&self, x: &[f32]) -> f32 {
        if x.len() != 16 * 96 {
            return 0.0;
        }
        let mut out = 0.0f32;
        // SAFETY: размеры проверены при загрузке.
        unsafe {
            if (self.tflite.copy_in)(self.input, x.as_ptr().cast(), x.len() * 4) != 0 {
                return 0.0;
            }
            if (self.tflite.invoke)(self.interp) != 0 {
                return 0.0;
            }
            if (self.tflite.copy_out)(
                self.output,
                (&mut out as *mut f32).cast(),
                std::mem::size_of::<f32>(),
            ) != 0
            {
                return 0.0;
            }
        }
        out
    }
}

impl Drop for TfliteHead {
    fn drop(&mut self) {
        // SAFETY: указатели созданы в load и ещё не освобождены.
        unsafe {
            (self.tflite.interp_delete)(self.interp);
            (self.tflite.model_delete)(self.model);
        }
    }
}

#[derive(Debug, Deserialize)]
struct Manifest {
    wake_word: String,
    model: String,
    #[serde(default = "default_type")]
    r#type: String,
    micro: Micro,
}

fn default_type() -> String {
    "micro".into()
}

#[derive(Debug, Deserialize)]
struct Micro {
    #[serde(default = "default_cutoff")]
    probability_cutoff: f32,
    #[serde(default = "default_window")]
    sliding_window_size: usize,
    #[serde(default = "default_step")]
    #[allow(dead_code)]
    feature_step_size: usize,
}

fn default_cutoff() -> f32 {
    0.9
}
fn default_window() -> usize {
    10
}
fn default_step() -> usize {
    10
}

pub struct MicroWakeWord {
    name: String,
    manifest_path: PathBuf,
    tflite: Tflite,
    model: *mut TfModel,
    interp: *mut TfInterpreter,
    input: *mut TfTensor,
    output: *mut TfTensor,
    frontend: *mut MfHandle,
    pending: Vec<i16>,
    tensor: [i8; 120],
    stride: usize,
    stride_max: usize,
    cutoff: u8,
    window: Vec<u8>,
    pos: usize,
    hit: bool,
    refractory_ms: u64,
    last_hit: Option<u64>,
    threshold: f32,
}

// SAFETY: экземпляр используется одним потоком; C-указатели не разделяются.
unsafe impl Send for MicroWakeWord {}

impl MicroWakeWord {
    pub fn load(
        manifest: &Path,
        threshold_override: Option<f32>,
        sensitivity: f32,
        refractory_ms: u64,
    ) -> Result<Self> {
        let text =
            std::fs::read_to_string(manifest).with_context(|| manifest.display().to_string())?;
        let m: Manifest = serde_json::from_str(&text)
            .with_context(|| format!("{}: манифест", manifest.display()))?;
        if m.r#type != "micro" {
            bail!("{}: ожидался type=micro", manifest.display());
        }
        let dir = manifest.parent().unwrap_or(Path::new("."));
        let model_path = dir.join(&m.model);
        if !model_path.is_file() {
            bail!("нет TFLite-модели: {}", model_path.display());
        }
        let vendor = library_path() == Path::new(TFLITE_VENDOR);
        let tflite = Tflite::load()?;
        let model_bytes =
            std::fs::read(&model_path).with_context(|| model_path.display().to_string())?;
        if vendor {
            if let (Some(need), Some(have)) =
                (model_min_runtime(&model_bytes), tflite.runtime_version())
            {
                if need > have {
                    bail!(
                        "модель требует TFLite {}.{}, прошивка {}.{}; \
                         используйте openWakeWord-модель",
                        need.0,
                        need.1,
                        have.0,
                        have.1
                    );
                }
            }
        }
        // SAFETY: валидный путь; модель живёт, пока жив MicroWakeWord.
        let (model, interp) = unsafe {
            let cpath = CString::new(model_path.to_string_lossy().as_bytes())
                .with_context(|| model_path.display().to_string())?;
            let model = (tflite.model_create_file)(cpath.as_ptr());
            if model.is_null() {
                bail!("TfLiteModelCreateFromFile: {model_path:?}");
            }
            let options = (tflite.options_create)();
            let interp = (tflite.interp_create)(model, options);
            (tflite.options_delete)(options);
            if interp.is_null() {
                (tflite.model_delete)(model);
                bail!("TfLiteInterpreterCreate");
            }
            if (tflite.allocate)(interp) != 0 {
                (tflite.interp_delete)(interp);
                (tflite.model_delete)(model);
                bail!("TfLiteInterpreterAllocateTensors");
            }
            (model, interp)
        };
        let input = unsafe { (tflite.input)(interp, 0) };
        let output = unsafe { (tflite.output)(interp, 0) };
        if input.is_null() || output.is_null() {
            unsafe {
                (tflite.interp_delete)(interp);
                (tflite.model_delete)(model);
            }
            bail!("TFLite: нет input/output");
        }
        // SAFETY: фронтенд C API, состояние создаётся один раз.
        let frontend = unsafe { mf_create() };
        if frontend.is_null() {
            unsafe {
                (tflite.interp_delete)(interp);
                (tflite.model_delete)(model);
            }
            bail!("microfrontend: mf_create");
        }
        let base = threshold_override.unwrap_or(m.micro.probability_cutoff) + sensitivity;
        let threshold = base.clamp(0.05, 0.999);
        let cutoff = (threshold * 255.0).round().clamp(0.0, 255.0) as u8;
        let stride_max = 3usize;
        Ok(Self {
            name: m.wake_word,
            manifest_path: manifest.to_path_buf(),
            tflite,
            model,
            interp,
            input,
            output,
            frontend,
            pending: Vec::new(),
            tensor: [0; 120],
            stride: 0,
            stride_max,
            cutoff,
            window: vec![0; m.micro.sliding_window_size.max(1)],
            pos: 0,
            hit: false,
            refractory_ms,
            last_hit: None,
            threshold,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn model_path(&self) -> &Path {
        &self.manifest_path
    }

    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    pub fn last_score(&self) -> f32 {
        self.window.iter().map(|v| u16::from(*v)).sum::<u16>() as f32
            / (255.0 * self.window.len().max(1) as f32)
    }

    pub fn reset(&mut self) {
        // SAFETY: указатели валидны; сбрасываем состояние фронтенда и окно.
        unsafe { mf_reset(self.frontend) };
        self.pending.clear();
        self.tensor.fill(0);
        self.stride = 0;
        self.window.fill(0);
        self.pos = 0;
        self.hit = false;
        self.last_hit = None;
    }

    pub fn push(&mut self, pcm: &[i16], now_ms: u64) -> Vec<Detection> {
        self.pending.extend_from_slice(pcm);
        let mut out = Vec::new();
        while self.pending.len() >= 160 {
            let chunk: Vec<i16> = self.pending.drain(..160).collect();
            let mut features = [0i8; 40];
            // SAFETY: frontend валиден, chunk 160 samples.
            let ok = unsafe {
                mf_process(
                    self.frontend,
                    chunk.as_ptr(),
                    chunk.len(),
                    features.as_mut_ptr(),
                )
            };
            if ok == 0 {
                continue;
            }
            self.feed_features(&features, now_ms, &mut out);
        }
        out
    }

    fn feed_features(&mut self, features: &[i8; 40], now_ms: u64, out: &mut Vec<Detection>) {
        self.tensor[self.stride * 40..(self.stride + 1) * 40].copy_from_slice(features);
        // SAFETY: tensor на 120 int8.
        unsafe {
            if (self.tflite.copy_in)(self.input, self.tensor.as_ptr().cast(), self.tensor.len()) != 0 {
                return;
            }
        }
        self.stride = (self.stride + 1) % self.stride_max;
        if self.stride != 0 {
            return;
        }
        // SAFETY: интерпретатор и output валидны.
        let rc = unsafe { (self.tflite.invoke)(self.interp) };
        if rc != 0 {
            return;
        }
        let mut p = 0u8;
        // SAFETY: output uint8[1].
        let rc = unsafe { (self.tflite.copy_out)(self.output, (&mut p as *mut u8).cast(), 1) };
        if rc != 0 {
            return;
        }
        self.window[self.pos] = p;
        self.pos = (self.pos + 1) % self.window.len();
        let sum: u32 = self.window.iter().map(|v| u32::from(*v)).sum();
        let need = u32::from(self.cutoff) * self.window.len() as u32;
        if sum > need {
            if !self.hit {
                let allowed = match self.last_hit {
                    Some(t) => now_ms.saturating_sub(t) >= self.refractory_ms,
                    None => true,
                };
                if allowed {
                    self.last_hit = Some(now_ms);
                    out.push(Detection {
                        score: p as f32 / 255.0,
                        at_ms: now_ms,
                    });
                }
            }
            self.hit = true;
        } else if sum < need / 2 {
            self.hit = false;
        }
    }
}

impl Drop for MicroWakeWord {
    fn drop(&mut self) {
        // SAFETY: ресурсы созданы в load и освобождаются один раз.
        unsafe {
            if !self.frontend.is_null() {
                mf_free(self.frontend);
            }
            if !self.interp.is_null() {
                (self.tflite.interp_delete)(self.interp);
            }
            if !self.model.is_null() {
                (self.tflite.model_delete)(self.model);
            }
        }
    }
}

/// Проверка, что манифест — microWakeWord (без загрузки TFLite).
pub fn is_micro_manifest(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned))
        .as_deref()
        == Some("micro")
}
