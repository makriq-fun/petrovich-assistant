//! Разбиение потокового текста LLM на фразы для TTS.

/// Накапливает дельты и отдаёт законченные предложения. Небольшая задержка
/// окупается тем, что TTS начинает говорить до конца ответа.
pub struct SentenceChunker {
    buf: String,
    min_chars: usize,
    max_chars: usize,
    in_code: bool,
    code_seen: bool,
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
            in_code: false,
            code_seen: false,
        }
    }

    /// Был ли в потоке код, который не попал в озвучку.
    pub fn code_seen(&self) -> bool {
        self.code_seen
    }

    /// Добавляет текст; завершённые фразы складывает в `out`.
    pub fn push(&mut self, delta: &str, out: &mut Vec<String>) {
        self.buf.push_str(delta);
        while let Some(cut) = self.find_boundary() {
            let sentence: String = self.buf.drain(..cut).collect();
            self.emit(&sentence, out);
        }
        if self.buf.chars().count() >= self.max_chars {
            let sentence = std::mem::take(&mut self.buf);
            self.emit(&sentence, out);
        }
    }

    pub fn flush(&mut self) -> Option<String> {
        let sentence = std::mem::take(&mut self.buf);
        let mut out = Vec::new();
        self.emit(&sentence, &mut out);
        out.into_iter().next()
    }

    fn emit(&mut self, sentence: &str, out: &mut Vec<String>) {
        let filtered = strip_code(sentence, &mut self.in_code, &mut self.code_seen);
        let cleaned = clean(&filtered);
        if !cleaned.is_empty() {
            out.push(cleaned);
        }
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

/// Убирает fenced-блоки и строки, похожие на код. Модель может нарушить
/// инструкцию, но пользователь не должен слышать код в озвучке.
fn strip_code(text: &str, in_code: &mut bool, seen: &mut bool) -> String {
    let mut outside = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("```") {
        if !*in_code {
            outside.push_str(&rest[..pos]);
        }
        *seen = true;
        *in_code = !*in_code;
        rest = &rest[pos + 3..];
    }
    if !*in_code {
        outside.push_str(rest);
    }

    let mut out = String::with_capacity(outside.len());
    for line in outside.split_inclusive('\n') {
        if is_code_line(line) {
            *seen = true;
        } else {
            out.push_str(line);
        }
    }
    out
}

fn is_code_line(line: &str) -> bool {
    let t = line.trim_start();
    const PREFIXES: &[&str] = &[
        "def ",
        "class ",
        "import ",
        "from ",
        "async def ",
        "function ",
        "func ",
        "fn ",
        "const ",
        "let ",
        "var ",
        "#include",
        "package ",
        "public class",
        "private ",
        "protected ",
        "return ",
        "print(",
        "printf(",
        "console.log",
        "SELECT ",
        "INSERT ",
        "UPDATE ",
        "DELETE ",
        "CREATE TABLE",
        "<?",
        "#!/",
        "curl ",
        "sudo ",
        "apt ",
        "pip install",
        "npm install",
        "docker ",
    ];
    PREFIXES.iter().any(|p| t.starts_with(p))
        || matches!(t.trim_end(), "}" | "};" | "{" | "} else {")
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
    fn drops_fenced_code_from_speech() {
        let mut c = SentenceChunker::new(1, 400);
        let mut out = Vec::new();
        c.push(
            "Вот пример:\n```python\ndef add(a, b):\n    return a + b\n```\nГотово.",
            &mut out,
        );
        if let Some(s) = c.flush() {
            out.push(s);
        }
        let all = out.join(" ");
        assert!(c.code_seen());
        assert!(all.contains("Вот пример"), "{all}");
        assert!(all.contains("Готово"), "{all}");
        assert!(!all.contains("def"), "{all}");
        assert!(!all.contains("return"), "{all}");
        assert!(!all.contains("python"), "{all}");
    }

    #[test]
    fn code_only_leaves_no_speech() {
        let mut c = SentenceChunker::new(1, 400);
        let mut out = Vec::new();
        c.push("```python\nprint(1)\n```\n", &mut out);
        assert!(out.is_empty(), "{out:?}");
        assert!(c.flush().is_none());
        assert!(c.code_seen());
    }

    #[test]
    fn unclosed_fence_is_still_code() {
        let mut c = SentenceChunker::new(1, 400);
        let mut out = Vec::new();
        c.push("```python\ndef add(a, b):\n", &mut out);
        if let Some(s) = c.flush() {
            out.push(s);
        }
        assert!(out.is_empty(), "{out:?}");
        assert!(c.code_seen());
    }

    #[test]
    fn drops_unfenced_code_lines() {
        let mut c = SentenceChunker::new(1, 400);
        let mut out = Vec::new();
        c.push(
            "Вот:\ndef add(a, b):\n    return a + b\n\nГотово.",
            &mut out,
        );
        if let Some(s) = c.flush() {
            out.push(s);
        }
        let all = out.join(" ");
        assert!(all.contains("Вот"), "{all}");
        assert!(all.contains("Готово"), "{all}");
        assert!(!all.contains("def"), "{all}");
        assert!(!all.contains("return"), "{all}");
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
