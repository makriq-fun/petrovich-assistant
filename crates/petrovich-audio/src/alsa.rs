//! libasound через dlopen: прошивка хранит свою копию вне пути загрузчика,
//! а линковать ALSA на хосте ради кросс-сборки не нужно.
//!
//! Подход и обработка ошибок совпадают с pulsar-alsa (AGPL-3.0-only,
//! Morvex885/pulsar), но здесь оставлен только нужный минимум.

use std::ffi::{c_char, c_int, c_long, c_uint, c_void, CStr, CString};
use std::path::Path;
use std::ptr;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use libloading::Library;

const SND_PCM_STREAM_PLAYBACK: c_int = 0;
const SND_PCM_STREAM_CAPTURE: c_int = 1;
const SND_PCM_FORMAT_S16_LE: c_int = 2;
const SND_PCM_ACCESS_RW_INTERLEAVED: c_int = 3;

type PcmOpen = unsafe extern "C" fn(*mut *mut c_void, *const c_char, c_int, c_int) -> c_int;
type PcmClose = unsafe extern "C" fn(*mut c_void) -> c_int;
type PcmSetParams =
    unsafe extern "C" fn(*mut c_void, c_int, c_int, c_uint, c_uint, c_int, c_uint) -> c_int;
type PcmReadi = unsafe extern "C" fn(*mut c_void, *mut c_void, c_long) -> c_long;
type PcmWritei = unsafe extern "C" fn(*const c_void, *const c_void, c_long) -> c_long;
type PcmRecover = unsafe extern "C" fn(*mut c_void, c_int, c_int) -> c_int;
type PcmDrop = unsafe extern "C" fn(*mut c_void) -> c_int;
type PcmDrain = unsafe extern "C" fn(*mut c_void) -> c_int;
type PcmPrepare = unsafe extern "C" fn(*mut c_void) -> c_int;
type StrError = unsafe extern "C" fn(c_int) -> *const c_char;

struct Alsa {
    _lib: Library,
    open: PcmOpen,
    close: PcmClose,
    set_params: PcmSetParams,
    readi: PcmReadi,
    writei: PcmWritei,
    recover: PcmRecover,
    drop_: PcmDrop,
    drain: PcmDrain,
    prepare: PcmPrepare,
    strerror: StrError,
}

impl Alsa {
    fn load(path: Option<&Path>) -> Result<Self> {
        let mut errors = Vec::new();
        let mut lib = None;
        let candidates: Vec<String> = match path {
            Some(p) if !p.as_os_str().is_empty() => vec![p.display().to_string()],
            _ => vec!["libasound.so.2".into()],
        };
        for name in candidates {
            // SAFETY: загрузка библиотеки без вызова её кода; символы проверяются ниже.
            match unsafe { Library::new(&name) } {
                Ok(l) => {
                    lib = Some(l);
                    break;
                }
                Err(e) => errors.push(format!("{name}: {e}")),
            }
        }
        let lib = lib.ok_or_else(|| anyhow!("libasound не загружена: {}", errors.join("; ")))?;
        unsafe fn sym<T: Copy>(lib: &Library, name: &[u8]) -> Result<T> {
            // SAFETY: T — точный тип символа; библиотека живёт в Alsa.
            unsafe { lib.get::<T>(name) }
                .map(|s| *s)
                .map_err(|e| anyhow!("{}: {e}", String::from_utf8_lossy(name)))
        }
        // SAFETY: типы символов соответствуют документированным прототипам ALSA.
        unsafe {
            Ok(Self {
                open: sym(&lib, b"snd_pcm_open\0")?,
                close: sym(&lib, b"snd_pcm_close\0")?,
                set_params: sym(&lib, b"snd_pcm_set_params\0")?,
                readi: sym(&lib, b"snd_pcm_readi\0")?,
                writei: sym(&lib, b"snd_pcm_writei\0")?,
                recover: sym(&lib, b"snd_pcm_recover\0")?,
                drop_: sym(&lib, b"snd_pcm_drop\0")?,
                drain: sym(&lib, b"snd_pcm_drain\0")?,
                prepare: sym(&lib, b"snd_pcm_prepare\0")?,
                strerror: sym(&lib, b"snd_strerror\0")?,
                _lib: lib,
            })
        }
    }

    fn text(&self, err: c_int) -> String {
        // SAFETY: snd_strerror возвращает статическую строку или NULL.
        let p = unsafe { (self.strerror)(err) };
        if p.is_null() {
            return format!("ALSA {err}");
        }
        // SAFETY: указатель на NUL-терминированную строку, живую во время вызова.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

/// PCM-поток ALSA. Не `Sync`: используется одним потоком.
pub struct Pcm {
    alsa: Arc<Alsa>,
    handle: *mut c_void,
    frames: usize,
    channels: usize,
    rate: u32,
    capture: bool,
}

// SAFETY: handle используется только из потока-владельца; Pcm не реализует Sync.
unsafe impl Send for Pcm {}

impl Pcm {
    pub fn open(
        lib_path: Option<&Path>,
        name: &str,
        rate: u32,
        channels: u32,
        period_ms: u32,
        capture: bool,
    ) -> Result<Self> {
        let alsa = Arc::new(Alsa::load(lib_path).context("загрузка libasound")?);
        let cname = CString::new(name).map_err(|_| anyhow!("NUL в имени PCM"))?;
        let stream = if capture {
            SND_PCM_STREAM_CAPTURE
        } else {
            SND_PCM_STREAM_PLAYBACK
        };
        let mut handle: *mut c_void = ptr::null_mut();
        // SAFETY: валидные C-строки и out-указатель; библиотека загружена.
        let rc = unsafe { (alsa.open)(&mut handle, cname.as_ptr(), stream, 0) };
        if rc < 0 {
            bail!("snd_pcm_open({name}): {}", alsa.text(rc));
        }
        let latency_us = 200_000;
        // SAFETY: handle открыт; параметры проверяются библиотекой.
        let rc = unsafe {
            (alsa.set_params)(
                handle,
                SND_PCM_FORMAT_S16_LE,
                SND_PCM_ACCESS_RW_INTERLEAVED,
                channels,
                rate,
                1,
                latency_us,
            )
        };
        if rc < 0 {
            // SAFETY: handle открыт этим вызовом.
            unsafe { (alsa.close)(handle) };
            bail!("snd_pcm_set_params({name}): {}", alsa.text(rc));
        }
        // 200 мс буфера при 20 мс периоде — 10 периодов; читаем/пишем по одному периоду.
        let frames = ((u64::from(rate) * u64::from(period_ms)) / 1000).max(1) as usize;
        Ok(Self {
            alsa,
            handle,
            frames,
            channels: channels as usize,
            rate,
            capture,
        })
    }

    pub fn frames_per_period(&self) -> usize {
        self.frames
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Читает один период interleaved S16. Возвращает число кадров.
    pub fn read(&mut self, buf: &mut [i16]) -> Result<usize> {
        let want = self.frames.min(buf.len() / self.channels);
        loop {
            // SAFETY: буфер валиден и вмещает want*channels сэмплов.
            let rc =
                unsafe { (self.alsa.readi)(self.handle, buf.as_mut_ptr().cast(), want as c_long) };
            if rc >= 0 {
                return Ok(rc as usize);
            }
            // SAFETY: handle открыт; recover безопасен для этого кода ошибки.
            let rc2 = unsafe { (self.alsa.recover)(self.handle, rc as c_int, 1) };
            if rc2 < 0 {
                bail!("snd_pcm_readi: {}", self.alsa.text(rc2));
            }
        }
    }

    /// Пишет interleaved S16; при переполнении восстанавливает поток.
    pub fn write(&mut self, samples: &[i16]) -> Result<()> {
        let mut off = 0;
        while off < samples.len() {
            let left = (samples.len() - off) / self.channels;
            if left == 0 {
                break;
            }
            let n = left.min(self.frames) as c_long;
            // SAFETY: указатель на валидный срез из n*channels сэмплов.
            let rc = unsafe { (self.alsa.writei)(self.handle, samples[off..].as_ptr().cast(), n) };
            if rc >= 0 {
                off += rc as usize * self.channels;
                continue;
            }
            // SAFETY: handle открыт.
            let rc2 = unsafe { (self.alsa.recover)(self.handle, rc as c_int, 1) };
            if rc2 < 0 {
                bail!("snd_pcm_writei: {}", self.alsa.text(rc2));
            }
        }
        Ok(())
    }

    /// Доигрывает буфер и готовит поток к следующему звуку.
    pub fn drain(&mut self) -> Result<()> {
        // SAFETY: handle открыт; drain блокирует до опустошения буфера.
        let rc = unsafe { (self.alsa.drain)(self.handle) };
        if rc < 0 {
            bail!("snd_pcm_drain: {}", self.alsa.text(rc));
        }
        // SAFETY: handle открыт.
        unsafe { (self.alsa.prepare)(self.handle) };
        Ok(())
    }

    /// Немедленно выбрасывает буфер: нужно для прерывания ответа.
    pub fn stop(&mut self) {
        // SAFETY: handle открыт.
        unsafe {
            (self.alsa.drop_)(self.handle);
            (self.alsa.prepare)(self.handle);
        }
    }

    pub fn is_capture(&self) -> bool {
        self.capture
    }
}

impl Drop for Pcm {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: handle открыт ровно один раз в Pcm::open и закрывается здесь один раз.
            unsafe { (self.alsa.close)(self.handle) };
            self.handle = ptr::null_mut();
        }
    }
}

/// Удаляет SysV-сегменты `dmix` (ipc_key 1024), оставшиеся от умершего
/// сервера. Иначе первый `snd_pcm_writei` нового клиента навсегда
/// застревает в `poll`: усилитель не включается, звука нет.
pub fn cleanup_stale_dmix() {
    let shm = std::fs::read_to_string("/proc/sysvipc/shm").unwrap_or_default();
    let sem = std::fs::read_to_string("/proc/sysvipc/sem").unwrap_or_default();
    let ids = stale_dmix_ids(&shm, &sem, |pid| {
        pid > 0 && Path::new(&format!("/proc/{pid}")).exists()
    });
    for shmid in ids.shm {
        // SAFETY: shmid взят из /proc/sysvipc/shm; IPC_RMID не читает buf.
        let rc = unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
        if rc == 0 {
            tracing::info!("dmix: удалён устаревший shm {shmid}");
        } else {
            tracing::warn!(
                "dmix: не удалось удалить shm {shmid}: {}",
                std::io::Error::last_os_error()
            );
        }
    }
    for semid in ids.sem {
        // SAFETY: semid взят из /proc/sysvipc/sem; semnum 0.
        let rc = unsafe { libc::semctl(semid, 0, libc::IPC_RMID) };
        if rc == 0 {
            tracing::info!("dmix: удалён устаревший sem {semid}");
        } else {
            tracing::warn!(
                "dmix: не удалось удалить sem {semid}: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct StaleDmix {
    shm: Vec<i32>,
    sem: Vec<i32>,
}

fn stale_dmix_ids(shm_text: &str, sem_text: &str, alive: impl Fn(i32) -> bool) -> StaleDmix {
    struct Shm {
        key: i32,
        id: i32,
        cpid: i32,
        nattch: i32,
    }
    let entries: Vec<Shm> = shm_text
        .lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 5 {
                return None;
            }
            Some(Shm {
                key: parse_id(f[0])?,
                id: parse_id(f[1])?,
                cpid: parse_id(f[4])?,
                nattch: parse_id(f[6])?,
            })
        })
        .collect();
    let Some(creator) = entries.iter().find(|e| e.key == 1024) else {
        return StaleDmix::default();
    };
    // Живой создатель или хотя бы один живой клиент: трогать нельзя.
    // Удаление такого сегмента обрывает dmix у Spotya/другого клиента, и
    // освободившийся slave остаётся занятым до его перезапуска.
    if alive(creator.cpid)
        || entries
            .iter()
            .any(|e| (e.key == 1024 || e.key == 1025) && e.nattch > 0)
    {
        return StaleDmix::default();
    }
    let shm = entries
        .iter()
        .filter(|e| e.key == 1024 || e.key == 1025)
        .map(|e| e.id)
        .collect();
    let sem = sem_text
        .lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 2 || parse_id(f[0])? != 1024 {
                return None;
            }
            parse_id(f[1])
        })
        .collect();
    StaleDmix { shm, sem }
}

fn parse_id(text: &str) -> Option<i32> {
    text.parse().ok().or_else(|| {
        text.strip_prefix("0x")
            .and_then(|h| i32::from_str_radix(h, 16).ok())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_dmix_with_dead_creator_is_removed() {
        let shm = "\
key shmid perms size cpid lpid nattch uid gid cuid cgid atime dtime ctime rss swap
1024 10 666 496 999999 999999 0 0 0 0 0 0 0 0 4096 0
1025 11 666 61440 999999 999999 0 0 0 0 0 0 0 0 61440 0";
        let sem = "\
key semid perms nsems uid gid cuid cgid otime ctime
1024 20 666 1 0 0 0 0 0 0";
        let ids = stale_dmix_ids(shm, sem, |pid| pid != 999_999);
        assert_eq!(ids.shm, vec![10, 11]);
        assert_eq!(ids.sem, vec![20]);
    }

    #[test]
    fn detached_client_keeps_dmix_alive() {
        let shm = "\
key shmid perms size cpid lpid nattch uid gid cuid cgid atime dtime ctime rss swap
1024 10 666 496 999999 999999 0 0 0 0 0 0 0 0 4096 0
1025 11 666 61440 999999 999999 1 0 0 0 0 0 0 0 61440 0";
        let sem = "\
key semid perms nsems uid gid cuid cgid otime ctime
1024 20 666 1 0 0 0 0 0 0";
        let ids = stale_dmix_ids(shm, sem, |pid| pid != 999_999);
        assert!(
            ids.shm.is_empty(),
            "нельзя удалять сегмент с живым клиентом"
        );
        assert!(ids.sem.is_empty());
    }

    #[test]
    fn live_dmix_creator_is_kept() {
        let shm = "\
key shmid perms size cpid lpid nattch uid gid cuid cgid atime dtime ctime rss swap
1024 10 666 496 1234 1234 1 0 0 0 0 0 0 0 4096 0
1025 11 666 61440 1234 1234 1 0 0 0 0 0 0 0 61440 0";
        let ids = stale_dmix_ids(shm, "", |pid| pid == 1234);
        assert!(ids.shm.is_empty());
        assert!(ids.sem.is_empty());
    }

    #[test]
    fn missing_library_is_reported() {
        let err = Pcm::open(
            Some(Path::new("/nonexistent/libasound.so")),
            "default",
            48_000,
            2,
            20,
            true,
        );
        assert!(err.is_err());
    }
}
