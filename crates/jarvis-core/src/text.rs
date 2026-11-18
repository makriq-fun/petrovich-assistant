//! Разбиение потокового текста LLM на фразы для TTS.

/// Накапливает дельты и отдаёт законченные предложения. Небольшая задержка
/// окупается тем, что TTS начинает говорить до конца ответа.
pub struct SentenceChunker {
    buf: String,
    min_chars: usize,
    max_chars: usize,
}

impl Default for SentenceChunker {
    fn default() -> Self {
        Self::new(12, 400)
    }
}

impl SentenceChunker {
    pub fn new(min_chars: usize, max_chars: usize) -> Self {
        Self {
            buf: String::new(),
            min_chars,
            max_chars,
        }
    }

    /// Добавляет текст; завершённые фразы складывает в `out`.
    pub fn push(&mut self, delta: &str, out: &mut Vec<String>) {
        self.buf.push_str(delta);
        while let Some(cut) = self.find_boundary() {
            let sentence: String = self.buf.drain(..cut).collect();
            let cleaned = clean(&sentence);
            if !cleaned.is_empty() {
                out.push(cleaned);
            }
        }
        if self.buf.chars().count() >= self.max_chars {
            let sentence = std::mem::take(&mut self.buf);
            let cleaned = clean(&sentence);
            if !cleaned.is_empty() {
                out.push(cleaned);
            }
        }
    }

    pub fn flush(&mut self) -> Option<String> {
        let sentence = std::mem::take(&mut self.buf);
        let cleaned = clean(&sentence);
        (!cleaned.is_empty()).then_some(cleaned)
    }

    pub fn pending(&self) -> &str {
        &self.buf
    }

    fn find_boundary(&self) -> Option<usize> {
        let chars: Vec<(usize, char)> = self.buf.char_indices().collect();
        for (i, (byte, ch)) in chars.iter().enumerate() {
            if !matches!(ch, '.' | '!' | '?' | '…' | '\n') {
                continue;
            }
            let next = chars.get(i + 1).map(|(_, c)| *c);
            if !matches!(
                next,
                None | Some(' ') | Some('\n') | Some('\t') | Some('"') | Some('»')
            ) {
                continue;
            }
            if *ch == '.' && is_abbreviation(&self.buf[..*byte]) {
                continue;
            }
            let end = byte + ch.len_utf8();
            if self.buf[..end].chars().count() < self.min_chars {
                continue;
            }
            return Some(end);
        }
        None
    }
}

fn is_abbreviation(prefix: &str) -> bool {
    let word = prefix
        .rsplit(|c: char| c.is_whitespace() || "([{\"'".contains(c))
        .next()
        .unwrap_or("");
    matches!(
        word.to_lowercase().as_str(),
        "т" | "г" | "ул" | "д" | "п" | "пр" | "им" | "см" | "рис" | "стр"
    ) || word.chars().count() <= 1
}

/// Убирает служебную разметку, которая плохо звучит вслух.
pub fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' | '`' | '#' | '_' | '~' => {}
            '[' => {
                // [текст](ссылка) -> текст
                let mut inner = String::new();
                for c in chars.by_ref() {
                    if c == ']' {
                        break;
                    }
                    inner.push(c);
                }
                if chars.peek() == Some(&'(') {
                    for c in chars.by_ref() {
                        if c == ')' {
                            break;
                        }
                    }
                }
                out.push_str(&inner);
            }
            c if c.is_control() && c != '\n' && c != '\t' => {}
            c => out.push(c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_stream_into_sentences() {
        let mut c = SentenceChunker::new(5, 400);
        let mut out = Vec::new();
        c.push("Привет! Как", &mut out);
        assert_eq!(out, ["Привет!"]);
        c.push(" дела? Всё хорошо.", &mut out);
        assert_eq!(out, ["Привет!", "Как дела?", "Всё хорошо."]);
    }

    #[test]
    fn keeps_abbreviations_and_decimals() {
        let mut c = SentenceChunker::new(1, 400);
        let mut out = Vec::new();
        c.push("Это т. е. пример 3.14 и всё", &mut out);
        assert!(out.is_empty(), "{out:?}");
        c.push(".", &mut out);
        c.push(" Готово.", &mut out);
        assert_eq!(out, ["Это т. е. пример 3.14 и всё.", "Готово."]);
    }

    #[test]
    fn flushes_remainder() {
        let mut c = SentenceChunker::default();
        let mut out = Vec::new();
        c.push("без точки", &mut out);
        assert!(out.is_empty());
        assert_eq!(c.flush().as_deref(), Some("без точки"));
        assert!(c.flush().is_none());
    }

    #[test]
    fn cleans_markdown_and_links() {
        assert_eq!(clean("**Жирный** [текст](https://x)"), "Жирный текст");
        assert_eq!(clean("a\n\n b"), "a b");
    }

    #[test]
    fn splits_long_text_without_punctuation() {
        let mut c = SentenceChunker::new(1, 10);
        let mut out = Vec::new();
        c.push("abcdefghijklmno", &mut out);
        assert_eq!(out.len(), 1);
        assert!(out[0].len() >= 10);
    }
}
