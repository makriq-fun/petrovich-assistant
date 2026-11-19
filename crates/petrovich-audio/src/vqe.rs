//! VQE из прошивки через dlopen. Библиотеки Яндекса не входят в репозиторий;
//! при их отсутствии тракт переходит на собственный AEC/ресемплер.
//!
//! Сигнатуры восстановлены проектом Pulsar (AGPL-3.0-only,
//! Morvex885/pulsar); здесь используется только VQE, без спотеров.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::Path;
use std::ptr;
use std::sync::LazyLock;

use anyhow::{anyhow, bail, Context, Result};
use libloading::os::unix::{Library, RTLD_GLOBAL, RTLD_NOW};

pub(crate) const EVIL: &str = "/system/vendor/quasar/libs/libyandex_evil.so";
pub(crate) const TFLITE: &str = "/system/vendor/quasar/libs/libtflite.so";
pub(crate) const ICONV: &str = "/system/vendor/quasar/libs/libiconv.so";

/// Выход VQE «omni» — основной канал распознавания.
pub const VQE_OMNI: i32 = 6;

type P = *mut c_void;
type CfgCreate = unsafe extern "C" fn(*const c_char, *mut P) -> P;
type CfgFree = unsafe extern "C" fn(P);
type CfgInt = unsafe extern "C" fn(P) -> c_int;
type Create = unsafe extern "C" fn(P, *const c_char, usize, *const c_void, *mut P) -> P;
type Free = unsafe extern "C" fn(P);
type Process = unsafe extern "C" fn(P, *const f32, usize, *const f32, usize) -> P;
type Output = unsafe extern "C" fn(P, c_int, usize, *mut *const f32, *mut usize) -> P;
type ErrMsg = unsafe extern "C" fn(P) -> *const c_char;
type ErrFree = unsafe extern "C" fn(P);

struct Lib {
    cfg_create: CfgCreate,
    cfg_free: CfgFree,
    cfg_chunk: CfgInt,
    cfg_frame: CfgInt,
    create: Create,
    free: Free,
    process: Process,
    output: Output,
    errmsg: ErrMsg,
    errfree: ErrFree,
}

static LIB: LazyLock<std::result::Result<Lib, String>> = LazyLock::new(load);

fn dl_error(e: libloading::Error) -> String {
    use std::error::Error as _;
    e.source()
        .map(|s| s.to_string())
        .unwrap_or_else(|| e.to_string())
}

pub(crate) fn open_global(path: &str) -> std::result::Result<(), String> {
    // SAFETY: инициализаторы библиотек прошивки не требуют предусловий; они не выгружаются.
    let lib = unsafe { Library::open(Some(path), RTLD_NOW | RTLD_GLOBAL) }.map_err(dl_error)?;
    let _ = lib.into_raw();
    Ok(())
}

fn load() -> std::result::Result<Lib, String> {
    let _ = open_global(ICONV);
    open_global(TFLITE).map_err(|e| format!("libtflite: {e}"))?;
    // SAFETY: библиотека прошивки, инициализаторы без предусловий; не выгружается.
    let lib = unsafe { Library::open(Some(EVIL), RTLD_NOW | RTLD_GLOBAL) }
        .map_err(|e| format!("libyandex_evil: {}", dl_error(e)))?;
    unsafe fn sym<T: Copy>(lib: &Library, name: &str) -> std::result::Result<T, String> {
        // SAFETY: точный тип символа; библиотека живёт до конца процесса.
        unsafe { lib.get::<T>(name.as_bytes()) }
            .map(|s| *s)
            .map_err(|_| format!("нет символа {name}"))
    }
    // SAFETY: типы — восстановленные прототипы C API.
    let out = unsafe {
        Ok(Lib {
            cfg_create: sym(&lib, "YandexVqeC_Config_CreateFromPreset")?,
            cfg_free: sym(&lib, "YandexVqeC_Config_Free")?,
            cfg_chunk: sym(&lib, "YandexVqeC_Config_GetInputChunkSize")?,
            cfg_frame: sym(&lib, "YandexVqeC_Config_GetProcessingFrame")?,
            create: sym(&lib, "YandexVqeC_Create")?,
            free: sym(&lib, "YandexVqeC_Free")?,
            process: sym(&lib, "YandexVqeC_Process")?,
            output: sym(&lib, "YandexVqeC_GetOutputChannelData")?,
            errmsg: sym(&lib, "YandexVqeC_Error_GetMessage")?,
            errfree: sym(&lib, "YandexVqeC_Error_Free")?,
        })
    };
    let _ = lib.into_raw();
    out
}

fn text(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: указатель от библиотеки на NUL-терминированную строку.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

pub struct Vqe {
    lib: &'static Lib,
    cfg: P,
    vqe: P,
    chunk: usize,
    frame: usize,
    mic_samples: usize,
    spk_samples: usize,
}

// SAFETY: экземпляр используется одним потоком обработки; C++ демон делал так же.
unsafe impl Send for Vqe {}

impl Vqe {
    pub fn new(preset: &str, models: &Path, nmic: usize, nspk: usize) -> Result<Self> {
        let lib = LIB.as_ref().map_err(|e| anyhow!("VQE недоступен: {e}"))?;
        if nmic == 0 || nspk == 0 {
            bail!("VQE: {nmic} микрофонов и {nspk} референсов");
        }
        let preset = CString::new(preset).context("NUL в названии пресета")?;
        let models = CString::new(models.to_string_lossy().as_bytes())
            .map_err(|_| anyhow!("NUL в пути моделей"))?;
        let mut cfg: P = ptr::null_mut();
        // SAFETY: валидные строки и out-указатель.
        let err = unsafe { (lib.cfg_create)(preset.as_ptr(), &mut cfg) };
        if let Err(e) = Self::check(lib, err, "Config_CreateFromPreset") {
            if !cfg.is_null() {
                // SAFETY: config успела создаться до ошибки; освобождаем.
                unsafe { (lib.cfg_free)(cfg) };
            }
            return Err(e);
        }
        if cfg.is_null() {
            bail!("VQE: пустой config");
        }
        // SAFETY: config валиден.
        let chunk = unsafe { (lib.cfg_chunk)(cfg) };
        let frame = unsafe { (lib.cfg_frame)(cfg) };
        if chunk <= 0 || frame <= 0 {
            // SAFETY: config принадлежит этому объекту и ещё не передан в Vqe.
            unsafe { (lib.cfg_free)(cfg) };
            bail!("VQE: неверные размеры {chunk}/{frame}");
        }
        let mut vqe: P = ptr::null_mut();
        // SAFETY: config валиден, строка пути NUL-терминирована, out-указатель.
        let err = unsafe { (lib.create)(cfg, models.as_ptr(), 0, ptr::null(), &mut vqe) };
        if let Err(e) = Self::check(lib, err, "Create") {
            // SAFETY: config принадлежит этому объекту.
            unsafe { (lib.cfg_free)(cfg) };
            return Err(e);
        }
        if vqe.is_null() {
            // SAFETY: config принадлежит этому объекту.
            unsafe { (lib.cfg_free)(cfg) };
            bail!("VQE: пустой экземпляр");
        }
        Ok(Self {
            lib,
            cfg,
            vqe,
            chunk: chunk as usize,
            frame: frame as usize,
            mic_samples: chunk as usize * nmic,
            spk_samples: chunk as usize * nspk,
        })
    }

    fn check(lib: &Lib, err: P, what: &str) -> Result<()> {
        if err.is_null() {
            return Ok(());
        }
        // SAFETY: ошибка создана библиотекой и освобождается здесь один раз.
        let msg = text(unsafe { (lib.errmsg)(err) });
        // SAFETY: та же ошибка, ещё не освобождена.
        unsafe { (lib.errfree)(err) };
        bail!("VQE {what}: {msg}")
    }

    pub fn chunk(&self) -> usize {
        self.chunk
    }

    pub fn frame(&self) -> usize {
        self.frame
    }

    pub fn process(&mut self, mics: &[f32], spk: &[f32]) -> Result<()> {
        if mics.len() != self.mic_samples || spk.len() != self.spk_samples {
            bail!(
                "VQE: {} / {} сэмплов, нужно {} / {}",
                mics.len(),
                spk.len(),
                self.mic_samples,
                self.spk_samples
            );
        }
        // SAFETY: экземпляр валиден, буферы живут на время вызова.
        let err = unsafe {
            (self.lib.process)(self.vqe, mics.as_ptr(), mics.len(), spk.as_ptr(), spk.len())
        };
        Self::check(self.lib, err, "Process")
    }

    /// Выход канала `ty` последнего чанка; пусто, если пресет его не даёт.
    pub fn output(&self, ty: i32) -> Result<&[f32]> {
        let mut data: *const f32 = ptr::null();
        let mut len = 0usize;
        // SAFETY: экземпляр валиден, out-указатели на локальные переменные.
        let err = unsafe { (self.lib.output)(self.vqe, ty, 0, &mut data, &mut len) };
        Self::check(self.lib, err, "GetOutputChannelData")?;
        if data.is_null() || len == 0 {
            return Ok(&[]);
        }
        // SAFETY: библиотека отдаёт len floats, живущих до следующего Process,
        // который требует &mut self.
        Ok(unsafe { std::slice::from_raw_parts(data, len) })
    }
}

impl Drop for Vqe {
    fn drop(&mut self) {
        if !self.vqe.is_null() {
            // SAFETY: создан Create, освобождается один раз.
            unsafe { (self.lib.free)(self.vqe) };
        }
        if !self.cfg.is_null() {
            // SAFETY: создан Config_Create, освобождается после экземпляра.
            unsafe { (self.lib.cfg_free)(self.cfg) };
        }
    }
}

/// Проверка доступности без создания экземпляра (диагностика).
pub fn available() -> std::result::Result<(), String> {
    LIB.as_ref().map(|_| ()).map_err(Clone::clone)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_on_host_is_reported_not_panics() {
        // На ПК библиотек прошивки нет; главное — понятная ошибка.
        let r = Vqe::new("yandexmini_2", Path::new("/tmp"), 4, 2);
        assert!(r.is_err());
        assert!(available().is_err());
    }
}
