//! Извлечение читаемого текста из HTML без внешних парсеров.

/// Возвращает (заголовок, текст), обрезанный по границе символа.
pub fn extract(html: &str, max_chars: usize) -> (Option<String>, String) {
    let mut title = None;
    let mut body = String::with_capacity(html.len().min(max_chars * 2));
    let lower = html.to_ascii_lowercase();
    let mut i = 0usize;
    while i < html.len() {
        if html[i..].starts_with('<') {
            let Some(end) = html[i..].find('>') else {
                break;
            };
            let tag = &html[i + 1..i + end];
            let name: String = tag
                .trim_start_matches('/')
                .split(|c: char| c.is_whitespace() || c == '/')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            let closing = tag.starts_with('/');
            i += end + 1;
            match name.as_str() {
                "script" | "style" | "noscript" | "template" if !closing => {
                    let needle = format!("</{name}");
                    if let Some(pos) = lower[i..].find(&needle) {
                        i += pos;
                    } else {
                        break;
                    }
                }
                "title" if !closing => {
                    if let Some(pos) = lower[i..].find("</title") {
                        let raw = &html[i..i + pos];
                        title = Some(collapse(&decode(raw)));
                        i += pos;
                    }
                }
                "p" | "div" | "br" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
                | "section" | "article" | "header" | "footer" | "blockquote" => {
                    body.push('\n');
                }
                _ => {}
            }
            continue;
        }
        let end = html[i..].find('<').map(|p| i + p).unwrap_or(html.len());
        body.push_str(&decode(&html[i..end]));
        i = end;
    }
    let mut text = collapse(&body);
    if text.chars().count() > max_chars {
        text = text
            .chars()
            .take(max_chars)
            .collect::<String>()
            .trim_end()
            .to_owned();
        text.push('…');
    }
    (title.filter(|t| !t.is_empty()), text)
}

pub(crate) fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos..];
        let Some(semi) = after.find(';').filter(|p| *p <= 12) else {
            out.push('&');
            rest = &after[1..];
            continue;
        };
        let entity = &after[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                u32::from_str_radix(&entity[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
            }
            _ if entity.starts_with('#') => {
                entity[1..].parse::<u32>().ok().and_then(char::from_u32)
            }
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tags_scripts_and_styles() {
        let html = r#"<html><head><title>Заголовок &amp; тест</title>
            <style>body{color:red}</style><script>alert(1)</script></head>
            <body><h1>Привет</h1><p>Мир &lt;ок&gt; <b>жирный</b></p></body></html>"#;
        let (title, text) = extract(html, 1000);
        assert_eq!(title.as_deref(), Some("Заголовок & тест"));
        assert_eq!(text, "Привет Мир <ок> жирный");
        assert!(!text.contains("alert"));
        assert!(!text.contains("color"));
    }

    #[test]
    fn decodes_numeric_entities() {
        assert_eq!(decode("&#1055;&#x440;&#1080;"), "При");
    }

    #[test]
    fn truncates_on_char_boundary() {
        let html = "<p>абвгдеёжз</p>";
        let (_, text) = extract(html, 4);
        assert_eq!(text, "абвг…");
    }

    #[test]
    fn unknown_entity_is_kept() {
        assert_eq!(decode("a &unknown; b"), "a &unknown; b");
    }
}
