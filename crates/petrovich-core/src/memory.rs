//! Долговременная память: короткие факты, переживающие сброс диалога и
//! перезапуск демона.
//!
//! Файл небольшой и перезаписывается целиком атомарно: временный файл в том
//! же каталоге, затем `rename`. Права — `0600`, потому что факты могут быть
//! личными. Ошибки чтения при старте не роняют демон: память просто пуста,
//! а запись при следующем `remember` создаст файл заново.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::chat::{ChatMessage, Role};

/// Общая память для демона и инструментов.
pub type SharedMemory = Arc<RwLock<Memory>>;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("пустая запись")]
    Empty,
    #[error("запись длиннее {0} символов")]
    TooLong(usize),
    #[error("ошибка ввода-вывода: {0}")]
    Io(#[from] std::io::Error),
    #[error("ошибка сериализации: {0}")]
    Serialize(#[from] serde_json::Error),
}

#[derive(Debug, Serialize, Deserialize)]
struct MemoryFile {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default)]
    facts: Vec<String>,
}

fn default_version() -> u32 {
    1
}

pub struct Memory {
    path: PathBuf,
    facts: Vec<String>,
    max_entries: usize,
    max_chars: usize,
    prompt_chars: usize,
}

impl Memory {
    /// Читает файл, если он есть. Повреждённый файл не считается фатальным:
    /// предупреждение в журнал и пустая память.
    pub fn load(
        path: impl Into<PathBuf>,
        max_entries: usize,
        max_chars: usize,
        prompt_chars: usize,
    ) -> Self {
        let path = path.into();
        let mut memory = Self {
            path,
            facts: Vec::new(),
            max_entries: max_entries.max(1),
            max_chars: max_chars.max(1),
            prompt_chars: prompt_chars.min(max_chars),
        };
        match fs::read(&memory.path) {
            Ok(bytes) => match serde_json::from_slice::<MemoryFile>(&bytes) {
                Ok(file) => memory.facts = file.facts,
                Err(e) => tracing::warn!("память {}: {e}", memory.path.display()),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("память {}: {e}", memory.path.display()),
        }
        memory.sanitize();
        memory
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn facts(&self) -> &[String] {
        &self.facts
    }

    pub fn len(&self) -> usize {
        self.facts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.facts.is_empty()
    }

    /// Добавляет факт. `Ok(false)` — такой факт уже есть (без учёта регистра).
    /// Старые факты вытесняются, чтобы не выйти за лимиты.
    pub fn add(&mut self, text: &str) -> Result<bool, MemoryError> {
        let text = normalize(text);
        if text.is_empty() {
            return Err(MemoryError::Empty);
        }
        if text.chars().count() > self.max_chars {
            return Err(MemoryError::TooLong(self.max_chars));
        }
        let key = text.to_lowercase();
        if self.facts.iter().any(|f| f.to_lowercase() == key) {
            return Ok(false);
        }
        let mut facts = self.facts.clone();
        facts.push(text);
        while facts.len() > self.max_entries {
            facts.remove(0);
        }
        while chars(&facts) > self.max_chars {
            facts.remove(0);
        }
        self.persist(&facts)?;
        self.facts = facts;
        Ok(true)
    }

    /// Удаляет факты, содержащие `needle` (без учёта регистра). Пустая
    /// строка очищает всю память.
    pub fn forget(&mut self, needle: &str) -> Result<usize, MemoryError> {
        let needle = normalize(needle);
        let facts: Vec<String> = if needle.is_empty() {
            Vec::new()
        } else {
            let needle = needle.to_lowercase();
            self.facts
                .iter()
                .filter(|f| !f.to_lowercase().contains(&needle))
                .cloned()
                .collect()
        };
        let removed = self.facts.len() - facts.len();
        if removed == 0 {
            return Ok(0);
        }
        self.persist(&facts)?;
        self.facts = facts;
        Ok(removed)
    }

    /// Фрагмент для системного промпта: самые свежие факты в пределах
    /// `prompt_chars`.
    pub fn prompt_fragment(&self) -> Option<String> {
        if self.facts.is_empty() {
            return None;
        }
        let mut lines = Vec::new();
        let mut used = 0usize;
        for fact in self.facts.iter().rev() {
            let line = format!("- {fact}");
            let len = line.chars().count() + 1;
            if used + len > self.prompt_chars {
                if lines.is_empty() && self.prompt_chars > 4 {
                    let truncated: String = line
                        .chars()
                        .take(self.prompt_chars.saturating_sub(1))
                        .collect();
                    lines.push(format!("{truncated}…"));
                }
                break;
            }
            used += len;
            lines.push(line);
        }
        if lines.is_empty() {
            return None;
        }
        lines.reverse();
        Some(format!(
            "Долговременная память (факты о пользователе):\n{}",
            lines.join("\n")
        ))
    }

    /// Дописывает факты в системное сообщение или создаёт его.
    pub fn inject(&self, messages: &mut Vec<ChatMessage>) {
        let Some(fragment) = self.prompt_fragment() else {
            return;
        };
        if let Some(system) = messages.iter_mut().find(|m| m.role == Role::System) {
            let mut content = system.content.take().unwrap_or_default();
            if !content.trim().is_empty() {
                content.push_str("\n\n");
            }
            content.push_str(&fragment);
            system.content = Some(content);
        } else {
            messages.insert(0, ChatMessage::system(fragment));
        }
    }

    fn sanitize(&mut self) {
        let mut facts = Vec::new();
        for raw in std::mem::take(&mut self.facts) {
            let text = normalize(&raw);
            if text.is_empty() || text.chars().count() > self.max_chars {
                continue;
            }
            if facts
                .iter()
                .any(|f: &String| f.to_lowercase() == text.to_lowercase())
            {
                continue;
            }
            facts.push(text);
        }
        while facts.len() > self.max_entries {
            facts.remove(0);
        }
        while chars(&facts) > self.max_chars {
            facts.remove(0);
        }
        self.facts = facts;
    }

    fn persist(&self, facts: &[String]) -> Result<(), MemoryError> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let data = serde_json::to_vec_pretty(&MemoryFile {
            version: 1,
            facts: facts.to_vec(),
        })?;
        let tmp = self.path.with_extension("json.tmp");
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(&data)?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// Добавляет память в системное сообщение под общей блокировкой.
pub fn inject_shared(memory: &SharedMemory, messages: &mut Vec<ChatMessage>) {
    let guard = memory.read().unwrap_or_else(|e| e.into_inner());
    guard.inject(messages);
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn chars(facts: &[String]) -> usize {
    facts.iter().map(|f| f.chars().count()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &std::path::Path, entries: usize, chars: usize, prompt: usize) -> Memory {
        Memory::load(dir.join("memory.json"), entries, chars, prompt)
    }

    #[test]
    fn facts_persist_and_dedupe() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = store(dir.path(), 10, 1000, 500);
        assert!(m.add("  Любит   зелёный чай ").unwrap());
        assert!(!m.add("любит зелёный чай").unwrap());
        assert_eq!(m.facts(), &["Любит зелёный чай".to_owned()]);

        let reloaded = store(dir.path(), 10, 1000, 500);
        assert_eq!(reloaded.facts(), m.facts());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(reloaded.path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn bounded_by_entries_and_chars() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = store(dir.path(), 2, 20, 20);
        m.add("первый").unwrap();
        m.add("второй").unwrap();
        m.add("третий").unwrap();
        assert_eq!(m.facts(), &["второй".to_owned(), "третий".to_owned()]);
        assert!(matches!(
            m.add(&"х".repeat(21)),
            Err(MemoryError::TooLong(20))
        ));
        assert!(matches!(m.add("   "), Err(MemoryError::Empty)));
    }

    #[test]
    fn forget_by_substring_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = store(dir.path(), 10, 1000, 500);
        m.add("Любит зелёный чай").unwrap();
        m.add("Живёт в Москве").unwrap();
        assert_eq!(m.forget("чай").unwrap(), 1);
        assert_eq!(m.facts(), &["Живёт в Москве".to_owned()]);
        assert_eq!(m.forget("Москв").unwrap(), 1);
        assert!(m.is_empty());
        m.add("Снова факт").unwrap();
        assert_eq!(m.forget("").unwrap(), 1);
        assert!(store(dir.path(), 10, 1000, 500).is_empty());
    }

    #[test]
    fn prompt_uses_newest_with_budget() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = store(dir.path(), 10, 1000, 20);
        m.add("старый факт").unwrap();
        m.add("новый факт").unwrap();
        let fragment = m.prompt_fragment().unwrap();
        assert!(fragment.contains("новый факт"));
        assert!(!fragment.contains("старый факт"));
        assert!(fragment.starts_with("Долговременная память"));
    }

    #[test]
    fn inject_appends_to_system_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = store(dir.path(), 10, 1000, 200);
        m.add("Зовут Максим").unwrap();
        let mut messages = vec![ChatMessage::system("Ты — Джарвис.")];
        m.inject(&mut messages);
        assert_eq!(messages.len(), 1);
        let system = messages[0].content.as_deref().unwrap();
        assert!(system.starts_with("Ты — Джарвис."));
        assert!(system.contains("Зовут Максим"));
        assert_eq!(messages[0].role, Role::System);
    }
}
