use std::time::{Duration, Instant};

use crate::chat::{ChatMessage, Role};

/// История диалога с ограничениями по числу ходов, символам и времени.
///
/// Ход начинается с сообщения пользователя. При обрезке удаляется самый старый
/// ход целиком, поэтому вызов инструмента не остаётся без своего результата.
pub struct ConversationHistory {
    system: Option<ChatMessage>,
    messages: Vec<ChatMessage>,
    chars: usize,
    max_turns: usize,
    max_chars: usize,
    ttl: Duration,
    last_activity: Instant,
}

impl ConversationHistory {
    pub fn new(
        system_prompt: impl Into<String>,
        max_turns: usize,
        max_chars: usize,
        ttl: Duration,
    ) -> Self {
        let prompt = system_prompt.into();
        let system = (!prompt.trim().is_empty()).then(|| ChatMessage::system(prompt));
        Self {
            system,
            messages: Vec::new(),
            chars: 0,
            max_turns: max_turns.max(1),
            max_chars: max_chars.max(256),
            ttl,
            last_activity: Instant::now(),
        }
    }

    pub fn push(&mut self, message: ChatMessage) -> &mut Self {
        self.chars += message.chars();
        self.messages.push(message);
        self.last_activity = Instant::now();
        self.trim();
        self
    }

    pub fn push_user(&mut self, text: impl Into<String>) -> &mut Self {
        self.push(ChatMessage::user(text))
    }

    pub fn push_assistant(&mut self, text: impl Into<String>) -> &mut Self {
        self.push(ChatMessage::assistant(text))
    }

    pub fn push_tool_result(
        &mut self,
        call_id: impl Into<String>,
        name: impl Into<String>,
        text: impl Into<String>,
    ) -> &mut Self {
        self.push(ChatMessage::tool_result(call_id, name, text))
    }

    pub fn touch(&mut self) {
        self.last_activity = Instant::now();
    }

    pub fn is_expired_at(&self, now: Instant) -> bool {
        !self.messages.is_empty() && now.duration_since(self.last_activity) >= self.ttl
    }

    pub fn expire_if_idle(&mut self) -> bool {
        if self.is_expired_at(Instant::now()) {
            self.reset();
            true
        } else {
            false
        }
    }

    pub fn reset(&mut self) {
        self.messages.clear();
        self.chars = 0;
        self.last_activity = Instant::now();
    }

    /// Сообщения для запроса к LLM: системный промпт и история.
    pub fn snapshot(&self) -> Vec<ChatMessage> {
        let mut out = Vec::with_capacity(self.messages.len() + 1);
        if let Some(system) = &self.system {
            out.push(system.clone());
        }
        out.extend(self.messages.iter().cloned());
        out
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn chars(&self) -> usize {
        self.chars
    }

    /// Последняя реплика ассистента без учёта tool-сообщений.
    pub fn last_assistant_text(&self) -> Option<&str> {
        self.messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .and_then(|m| m.content.as_deref())
    }

    fn turns(&self) -> usize {
        self.messages
            .iter()
            .filter(|m| m.role == Role::User)
            .count()
    }

    fn trim(&mut self) {
        while !self.messages.is_empty()
            && (self.turns() > self.max_turns || self.chars > self.max_chars)
        {
            let end = self
                .messages
                .iter()
                .enumerate()
                .skip(1)
                .find_map(|(i, m)| (m.role == Role::User).then_some(i))
                .unwrap_or(self.messages.len());
            let removed: Vec<_> = self.messages.drain(..end).collect();
            self.chars -= removed.iter().map(ChatMessage::chars).sum::<usize>();
        }
        // Страховка: после обрезки история не должна начинаться с результата
        // инструмента без вызова.
        while self.messages.first().is_some_and(|m| m.role == Role::Tool) {
            let first = self.messages.remove(0);
            self.chars -= first.chars();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{FunctionCall, ToolCall};

    fn hist(turns: usize, chars: usize) -> ConversationHistory {
        ConversationHistory::new("система", turns, chars, Duration::from_secs(60))
    }

    #[test]
    fn snapshot_starts_with_system_prompt() {
        let mut h = hist(8, 4096);
        h.push_user("привет");
        let snap = h.snapshot();
        assert_eq!(snap[0].role, Role::System);
        assert_eq!(snap[1].content.as_deref(), Some("привет"));
    }

    #[test]
    fn trims_whole_oldest_turn_keeping_tool_pair() {
        let mut h = hist(2, 10_000);
        h.push_user("вопрос 1");
        h.push(ChatMessage {
            role: Role::Assistant,
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "call-1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "time".into(),
                    arguments: "{}".into(),
                },
            }]),
            tool_call_id: None,
            name: None,
        });
        h.push_tool_result("call-1", "time", "12:00");
        h.push_assistant("ответ 1");
        h.push_user("вопрос 2");
        h.push_assistant("ответ 2");
        h.push_user("вопрос 3");
        h.push_assistant("ответ 3");
        let snap = h.snapshot();
        assert!(snap.iter().all(|m| m.role != Role::Tool));
        assert_eq!(snap.iter().filter(|m| m.role == Role::User).count(), 2);
        assert_eq!(snap.last().unwrap().content.as_deref(), Some("ответ 3"));
    }

    #[test]
    fn trims_by_chars() {
        let mut h = hist(100, 300);
        h.push_user("a".repeat(200));
        h.push_assistant("b".repeat(200));
        h.push_user("c".repeat(200));
        assert!(h.chars() <= 300);
        assert!(h
            .snapshot()
            .iter()
            .any(|m| m.content.as_deref() == Some(&"c".repeat(200))));
    }

    #[test]
    fn expires_and_resets() {
        let mut h = ConversationHistory::new("s", 8, 4096, Duration::ZERO);
        h.push_user("привет");
        assert!(h.expire_if_idle());
        assert!(h.is_empty());
    }

    #[test]
    fn reset_keeps_system_prompt() {
        let mut h = hist(8, 4096);
        h.push_user("привет");
        h.reset();
        assert_eq!(h.snapshot().len(), 1);
        assert_eq!(h.snapshot()[0].role, Role::System);
    }
}
