//! Штатный command spotter из прошивки: быстрые команды без слова активации.
//!
//! Модель `spotter_models/command` загружается тем же `libyandex_evil.so`, что
//! и VQE. Библиотека и модель принадлежат прошивке и в репозиторий не входят.
//! Прототип C API восстановлен по `chatgpt-voice` и `yastationd`:
//! `spotter_conf_read(dir, &err)` → `spotter_create(conf, rate, 0, &err)` →
//! `spotter_submit_raw_data(handle, pcm, bytes, &err)` →
//! `spotter_get_next_event(handle, &err)`.

use std::ffi::{c_char, c_void, CStr, CString};
use std::path::Path;
use std::ptr;
use std::sync::LazyLock;

use anyhow::{anyhow, bail, Result};
use libloading::os::unix::{Library, RTLD_NOW};

use crate::vqe::{open_global, EVIL, ICONV, TFLITE};

/// Вендор принимает ровно 20 мс 16 кГц mono за вызов: 320 сэмплов / 640 байт.
const CHUNK: usize = 320;
const EVENTS: usize = 64;

type P = *mut c_void;
type ConfRead = unsafe extern "C" fn(*const c_char, *mut P) -> P;
type SampleRate = unsafe extern "C" fn(*const c_void, *mut P) -> u32;
type Create = unsafe extern "C" fn(*const c_void, u32, u32, *mut P) -> P;
type Submit = unsafe extern "C" fn(P, *const c_void, usize, *mut P);
type NextEvent = unsafe extern "C" fn(P, *mut P) -> i32;
type PhraseId = unsafe extern "C" fn(P, *mut P) -> i32;
type Phrase = unsafe extern "C" fn(P, *mut P) -> *const c_char;
type Destroy = unsafe extern "C" fn(P, *mut P);
type ConfFree = unsafe extern "C" fn(P, *mut P);
type ErrMsg = unsafe extern "C" fn(*const c_void) -> *const c_char;
type ErrFree = unsafe extern "C" fn(*mut P);

struct Lib {
    conf_read: ConfRead,
    sample_rate: SampleRate,
    create: Create,
    submit: Submit,
    next_event: NextEvent,
    phrase_id: PhraseId,
    phrase: Phrase,
    destroy: Destroy,
    conf_free: ConfFree,
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

fn load() -> std::result::Result<Lib, String> {
    open_global(ICONV).map_err(|e| format!("libiconv: {e}"))?;
    open_global(TFLITE).map_err(|e| format!("libtflite: {e}"))?;
    // SAFETY: библиотека прошивки; инициализаторы без предусловий, не выгружается.
    let lib = unsafe { Library::open(Some(EVIL), RTLD_NOW) }
        .map_err(|e| format!("libyandex_evil: {}", dl_error(e)))?;
    unsafe fn sym<T: Copy>(lib: &Library, name: &str) -> std::result::Result<T, String> {
        // SAFETY: точный тип символа; библиотека живёт до конца процесса.
        unsafe { lib.get::<T>(name.as_bytes()) }
            .map(|s| *s)
            .map_err(|_| format!("нет символа {name}"))
    }
    // SAFETY: типы восстановлены по C-обёртке станции.
    let out = unsafe {
        Ok(Lib {
            conf_read: sym(&lib, "spotter_conf_read")?,
            sample_rate: sym(&lib, "spotter_conf_native_sample_rate")?,
            create: sym(&lib, "spotter_create")?,
            submit: sym(&lib, "spotter_submit_raw_data")?,
            next_event: sym(&lib, "spotter_get_next_event")?,
            phrase_id: sym(&lib, "spotter_get_current_phrase_id")?,
            phrase: sym(&lib, "spotter_get_current_phrase")?,
            destroy: sym(&lib, "spotter_destroy")?,
            conf_free: sym(&lib, "spotter_conf_free")?,
            errmsg: sym(&lib, "spotter_error_get_message")?,
            errfree: sym(&lib, "spotter_error_free")?,
        })
    };
    let _ = lib.into_raw();
    out
}

fn take_error(lib: &Lib, mut err: P, operation: &str) -> Result<()> {
    if err.is_null() {
        return Ok(());
    }
    // SAFETY: указатель от библиотеки на NUL-терминированную строку.
    let message = unsafe {
        let p = (lib.errmsg)(err);
        if p.is_null() {
            "неизвестная ошибка".into()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    };
    // SAFETY: ошибку освобождает сама библиотека; out-указатель валиден.
    unsafe { (lib.errfree)(&mut err) };
    bail!("{operation}: {message}")
}

/// Одно распознанное действие. Всё, что не относится к музыке/громкости/стопу,
/// отбрасывается.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Next,
    Previous,
    Play,
    Stop,
    VolumeUp,
    VolumeDown,
}

/// Сопоставление строки/номера из модели. Группы `commands` в flags.txt
/// для одиночных слов совпадают с номерами в words.txt, поэтому id — резерв.
pub fn command_from_phrase(phrase: &str, id: i32) -> Option<Command> {
    let normalized: String = phrase
        .to_lowercase()
        .replace('ё', "е")
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    let normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    match normalized.as_str() {
        "дальше" | "следующий" => Some(Command::Next),
        "назад" => Some(Command::Previous),
        "продолжи" | "играй" => Some(Command::Play),
        "пауза" | "стоп" | "хватит" => Some(Command::Stop),
        "громче" => Some(Command::VolumeUp),
        "тише" => Some(Command::VolumeDown),
        _ => match id {
            1 | 15 => Some(Command::Next),
            2 => Some(Command::Previous),
            10 | 11 => Some(Command::Play),
            12..=14 => Some(Command::Stop),
            16 => Some(Command::VolumeUp),
            17 => Some(Command::VolumeDown),
            _ => None,
        },
    }
}

/// Загруженный вендорный spotter. Держит conf и detector; освобождает их в Drop.
pub struct CommandSpotter {
    lib: &'static Lib,
    conf: P,
    detector: P,
    buffer: Vec<i16>,
    failed: bool,
}

// SAFETY: объект используется одним потоком демона; вендорный API не потокобезопасен,
// но CommandSpotter не вызывается одновременно из нескольких потоков.
unsafe impl Send for CommandSpotter {}

impl CommandSpotter {
    pub fn open(dir: &Path) -> Result<Self> {
        let lib = LIB
            .as_ref()
            .map_err(|e| anyhow!("command spotter недоступен: {e}"))?;
        let path = CString::new(dir.to_string_lossy().as_bytes())
            .map_err(|_| anyhow!("NUL в пути модели {}", dir.display()))?;
        let mut err = ptr::null_mut();
        // SAFETY: валидный путь и out-указатель.
        let conf = unsafe { (lib.conf_read)(path.as_ptr(), &mut err) };
        if let Err(e) = take_error(lib, err, "spotter_conf_read") {
            unsafe { free_conf(lib, conf) };
            return Err(e);
        }
        if conf.is_null() {
            bail!("command spotter: пустой conf для {}", dir.display());
        }
        let mut err = ptr::null_mut();
        // SAFETY: conf валиден.
        let rate = unsafe { (lib.sample_rate)(conf, &mut err) };
        if let Err(e) = take_error(lib, err, "spotter_conf_native_sample_rate") {
            unsafe { free_conf(lib, conf) };
            return Err(e);
        }
        if rate != 16_000 {
            unsafe { free_conf(lib, conf) };
            bail!("command spotter: частота {rate}, ожидается 16000");
        }
        let mut err = ptr::null_mut();
        // SAFETY: conf валиден, 0 — опция по умолчанию.
        let detector = unsafe { (lib.create)(conf, rate, 0, &mut err) };
        if let Err(e) = take_error(lib, err, "spotter_create") {
            unsafe { free_conf(lib, conf) };
            return Err(e);
        }
        if detector.is_null() {
            unsafe { free_conf(lib, conf) };
            bail!("command spotter: пустой detector");
        }
        Ok(Self {
            lib,
            conf,
            detector,
            buffer: Vec::with_capacity(CHUNK),
            failed: false,
        })
    }

    /// Кормит 16 кГц mono; возвращает распознанные действия.
    pub fn feed(&mut self, pcm: &[i16]) -> Vec<Command> {
        let mut out = Vec::new();
        if self.failed {
            return out;
        }
        self.buffer.extend_from_slice(pcm);
        while self.buffer.len() >= CHUNK {
            let mut chunk = [0i16; CHUNK];
            chunk.copy_from_slice(&self.buffer[..CHUNK]);
            self.buffer.drain(..CHUNK);
            if let Err(e) = self.submit(&chunk) {
                tracing::warn!("command spotter: {e:#}; выключаю");
                self.failed = true;
                return out;
            }
            self.drain_events(&mut out);
        }
        out
    }

    fn submit(&self, chunk: &[i16; CHUNK]) -> Result<()> {
        let mut err = ptr::null_mut();
        // SAFETY: detector валиден, chunk — 320 i16 (640 байт).
        unsafe {
            (self.lib.submit)(
                self.detector,
                chunk.as_ptr().cast(),
                CHUNK * std::mem::size_of::<i16>(),
                &mut err,
            )
        };
        take_error(self.lib, err, "spotter_submit_raw_data")
    }

    fn drain_events(&mut self, out: &mut Vec<Command>) {
        for _ in 0..EVENTS {
            let mut err = ptr::null_mut();
            // SAFETY: detector валиден.
            let event = unsafe { (self.lib.next_event)(self.detector, &mut err) };
            if let Err(e) = take_error(self.lib, err, "spotter_get_next_event") {
                tracing::warn!("command spotter: {e:#}; выключаю");
                self.failed = true;
                return;
            }
            if event == 0 {
                return;
            }
            // 2 = CONFIRMATION_1ST_STAGE, 5 = CONFIRMATION_2ND_STAGE.
            if event != 2 && event != 5 {
                continue;
            }
            let mut err = ptr::null_mut();
            // SAFETY: detector валиден.
            let id = unsafe { (self.lib.phrase_id)(self.detector, &mut err) };
            if let Err(e) = take_error(self.lib, err, "spotter_get_current_phrase_id") {
                tracing::warn!("command spotter: {e:#}");
                continue;
            }
            let mut err = ptr::null_mut();
            // SAFETY: detector валиден.
            let p = unsafe { (self.lib.phrase)(self.detector, &mut err) };
            if let Err(e) = take_error(self.lib, err, "spotter_get_current_phrase") {
                tracing::warn!("command spotter: {e:#}");
                continue;
            }
            let phrase = if p.is_null() {
                String::new()
            } else {
                // SAFETY: указатель от библиотеки на NUL-терминированную строку.
                unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
            };
            if let Some(cmd) = command_from_phrase(&phrase, id) {
                out.push(cmd);
            }
        }
        tracing::warn!("command spotter: очередь событий не опустела");
    }
}

unsafe fn free_conf(lib: &Lib, conf: P) {
    if conf.is_null() {
        return;
    }
    let mut err = ptr::null_mut();
    // SAFETY: conf валиден и ещё не освобождён.
    unsafe { (lib.conf_free)(conf, &mut err) };
    if !err.is_null() {
        // SAFETY: ошибку освобождает библиотека.
        unsafe { (lib.errfree)(&mut err) };
    }
}

impl Drop for CommandSpotter {
    fn drop(&mut self) {
        // SAFETY: указатели созданы библиотекой и принадлежат этому объекту.
        unsafe {
            if !self.detector.is_null() {
                let mut err = ptr::null_mut();
                (self.lib.destroy)(self.detector, &mut err);
                if !err.is_null() {
                    (self.lib.errfree)(&mut err);
                }
            }
            free_conf(self.lib, self.conf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_music_and_volume_words() {
        assert_eq!(command_from_phrase("дальше", 0), Some(Command::Next));
        assert_eq!(command_from_phrase("следующий", 0), Some(Command::Next));
        assert_eq!(command_from_phrase("назад", 0), Some(Command::Previous));
        assert_eq!(command_from_phrase("продолжи", 0), Some(Command::Play));
        assert_eq!(command_from_phrase("играй", 0), Some(Command::Play));
        assert_eq!(command_from_phrase("пауза", 0), Some(Command::Stop));
        assert_eq!(command_from_phrase("стоп", 0), Some(Command::Stop));
        assert_eq!(command_from_phrase("хватит", 0), Some(Command::Stop));
        assert_eq!(command_from_phrase("громче", 0), Some(Command::VolumeUp));
        assert_eq!(command_from_phrase("тише", 0), Some(Command::VolumeDown));
    }

    #[test]
    fn falls_back_to_ids_and_ignores_other_phrases() {
        assert_eq!(command_from_phrase("", 14), Some(Command::Stop));
        assert_eq!(command_from_phrase("", 16), Some(Command::VolumeUp));
        assert_eq!(command_from_phrase("включи свет", 18), None);
        assert_eq!(command_from_phrase("выше", 3), None);
        assert_eq!(command_from_phrase("домой", 9), None);
        assert_eq!(command_from_phrase("ПАУЗА!", 0), Some(Command::Stop));
    }
}
