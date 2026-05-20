use crate::client::{ServerEvent, ToolCallStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppState {
    Connecting,
    Idle,
    Sending,
    Streaming,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    System,
    ToolCall,
}

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: String,
    pub tool_call_id: Option<String>,
    pub tool_status: Option<ToolCallStatus>,
}

pub struct App {
    pub state: AppState,
    pub messages: Vec<ChatMessage>,
    pub input: String,
    pub cursor_pos: usize,
    pub conversation_id: Option<String>,
    pub should_quit: bool,
    pub agent_type: String,
    pub model: Option<String>,
    // Phase 3 additions
    pub session_id: Option<String>,
    pub history: Vec<String>,
    pub history_index: Option<usize>,
    history_draft: String,
    pub scroll_offset: usize,
    pub user_scrolled: bool,
    pub cancel_requested: bool,
}

fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max.saturating_sub(3);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

impl App {
    pub fn new(agent_type: String, model: Option<String>) -> Self {
        Self {
            state: AppState::Connecting,
            messages: Vec::new(),
            input: String::new(),
            cursor_pos: 0,
            conversation_id: None,
            should_quit: false,
            agent_type,
            model,
            session_id: None,
            history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            scroll_offset: 0,
            user_scrolled: false,
            cancel_requested: false,
        }
    }

    pub fn handle_server_event(&mut self, event: ServerEvent) {
        match event {
            ServerEvent::Connected => {
                self.state = AppState::Idle;
            }
            ServerEvent::StreamText { content } => {
                self.state = AppState::Streaming;
                if let Some(last) = self.messages.last_mut()
                    && last.role == MessageRole::Assistant
                {
                    last.content.push_str(&content);
                } else {
                    self.messages.push(ChatMessage {
                        role: MessageRole::Assistant,
                        content,
                        tool_call_id: None,
                        tool_status: None,
                    });
                }
                if !self.user_scrolled {
                    self.scroll_offset = 0;
                }
            }
            ServerEvent::StreamThinking { .. } => {
                self.state = AppState::Streaming;
            }
            ServerEvent::ToolCall { call_id, name, status, input, output } => {
                self.state = AppState::Streaming;
                if let Some(existing) = self.messages.iter_mut().rev().find(|m| {
                    m.tool_call_id.as_deref() == Some(&call_id)
                }) {
                    existing.tool_status = Some(status);
                    match status {
                        ToolCallStatus::Completed => {
                            if let Some(out) = &output {
                                let summary = truncate_str(out, 80);
                                existing.content = format!("{name} -> {summary}");
                            } else {
                                existing.content = format!("{name} (done)");
                            }
                        }
                        ToolCallStatus::Error => {
                            let msg = output.as_deref().unwrap_or("failed");
                            existing.content = format!("{name} (error: {})", truncate_str(msg, 60));
                        }
                        ToolCallStatus::Running => {}
                    }
                } else {
                    let summary = input
                        .as_deref()
                        .map(|s| truncate_str(s, 60).to_string())
                        .unwrap_or_default();
                    let content = if summary.is_empty() {
                        name.clone()
                    } else {
                        format!("{name} {summary}")
                    };
                    self.messages.push(ChatMessage {
                        role: MessageRole::ToolCall,
                        content,
                        tool_call_id: Some(call_id),
                        tool_status: Some(status),
                    });
                }
                if !self.user_scrolled {
                    self.scroll_offset = 0;
                }
            }
            ServerEvent::StreamFinish => {
                self.state = AppState::Idle;
                self.cancel_requested = false;
                if !self.user_scrolled {
                    self.scroll_offset = 0;
                }
            }
            ServerEvent::StreamError { message } => {
                self.messages.push(ChatMessage {
                    role: MessageRole::System,
                    content: format!("[Error] {message}"),
                    tool_call_id: None,
                    tool_status: None,
                });
                self.state = AppState::Idle;
                self.cancel_requested = false;
            }
            ServerEvent::Disconnected => {
                self.messages.push(ChatMessage {
                    role: MessageRole::System,
                    content: "[Disconnected]".to_string(),
                    tool_call_id: None,
                    tool_status: None,
                });
                self.should_quit = true;
            }
        }
    }

    pub fn submit_input(&mut self) -> Option<String> {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return None;
        }
        self.history.push(text.clone());
        self.history_index = None;
        self.history_draft.clear();
        self.input.clear();
        self.cursor_pos = 0;
        self.cancel_requested = false;
        self.messages.push(ChatMessage {
            role: MessageRole::User,
            content: text.clone(),
            tool_call_id: None,
            tool_status: None,
        });
        self.state = AppState::Sending;
        if !self.user_scrolled {
            self.scroll_offset = 0;
        }
        Some(text)
    }

    pub fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor_pos, c);
        self.cursor_pos += c.len_utf8();
    }

    pub fn insert_newline(&mut self) {
        self.input.insert(self.cursor_pos, '\n');
        self.cursor_pos += 1;
    }

    pub fn input_line_count(&self) -> usize {
        self.input.lines().count().max(1)
    }

    pub fn delete_char(&mut self) {
        if self.cursor_pos > 0 {
            let prev = self.input[..self.cursor_pos]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.input.drain(prev..self.cursor_pos);
            self.cursor_pos = prev;
        }
    }

    pub fn move_cursor_left(&mut self) {
        if self.cursor_pos > 0 {
            self.cursor_pos = self.input[..self.cursor_pos]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    pub fn move_cursor_right(&mut self) {
        if self.cursor_pos < self.input.len() {
            self.cursor_pos = self.input[self.cursor_pos..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor_pos + i)
                .unwrap_or(self.input.len());
        }
    }

    pub fn clear_input(&mut self) {
        self.input.clear();
        self.cursor_pos = 0;
    }

    pub fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_index {
            None => {
                self.history_draft = self.input.clone();
                let idx = self.history.len() - 1;
                self.history_index = Some(idx);
                self.input = self.history[idx].clone();
                self.cursor_pos = self.input.len();
            }
            Some(idx) if idx > 0 => {
                let new_idx = idx - 1;
                self.history_index = Some(new_idx);
                self.input = self.history[new_idx].clone();
                self.cursor_pos = self.input.len();
            }
            _ => {}
        }
    }

    pub fn history_down(&mut self) {
        if let Some(idx) = self.history_index {
            if idx + 1 < self.history.len() {
                let new_idx = idx + 1;
                self.history_index = Some(new_idx);
                self.input = self.history[new_idx].clone();
                self.cursor_pos = self.input.len();
            } else {
                self.history_index = None;
                self.input = self.history_draft.clone();
                self.cursor_pos = self.input.len();
                self.history_draft.clear();
            }
        }
    }

    pub fn scroll_up(&mut self, amount: usize) {
        self.scroll_offset = self.scroll_offset.saturating_add(amount);
        self.user_scrolled = true;
    }

    pub fn scroll_down(&mut self, amount: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
        if self.scroll_offset == 0 {
            self.user_scrolled = false;
        }
    }

    #[allow(dead_code)]
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
        self.user_scrolled = false;
    }

    pub fn clear_messages(&mut self) {
        self.messages.clear();
        self.scroll_offset = 0;
        self.user_scrolled = false;
    }

    pub fn request_cancel(&mut self) {
        if self.state == AppState::Streaming {
            self.cancel_requested = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_app() -> App {
        App::new("claude".to_string(), None)
    }

    #[test]
    fn test_history_up_down() {
        let mut app = make_app();
        app.state = AppState::Idle;
        app.input = "first".to_string();
        app.cursor_pos = 5;
        app.submit_input();
        app.input = "second".to_string();
        app.cursor_pos = 6;
        app.submit_input();

        assert_eq!(app.history, vec!["first", "second"]);

        // Type something, then go up
        app.input = "draft".to_string();
        app.history_up();
        assert_eq!(app.input, "second");
        assert_eq!(app.history_index, Some(1));

        app.history_up();
        assert_eq!(app.input, "first");
        assert_eq!(app.history_index, Some(0));

        // Can't go further up
        app.history_up();
        assert_eq!(app.input, "first");

        // Go down
        app.history_down();
        assert_eq!(app.input, "second");

        app.history_down();
        assert_eq!(app.input, "draft");
        assert_eq!(app.history_index, None);
    }

    #[test]
    fn test_scroll_up_down() {
        let mut app = make_app();
        assert_eq!(app.scroll_offset, 0);
        assert!(!app.user_scrolled);

        app.scroll_up(5);
        assert_eq!(app.scroll_offset, 5);
        assert!(app.user_scrolled);

        app.scroll_down(3);
        assert_eq!(app.scroll_offset, 2);
        assert!(app.user_scrolled);

        app.scroll_down(10); // Should clamp to 0
        assert_eq!(app.scroll_offset, 0);
        assert!(!app.user_scrolled);
    }

    #[test]
    fn test_scroll_to_bottom() {
        let mut app = make_app();
        app.scroll_up(10);
        app.scroll_to_bottom();
        assert_eq!(app.scroll_offset, 0);
        assert!(!app.user_scrolled);
    }

    #[test]
    fn test_clear_messages() {
        let mut app = make_app();
        app.messages.push(ChatMessage {
            role: MessageRole::User,
            content: "hello".to_string(),
            tool_call_id: None,
            tool_status: None,
        });
        app.scroll_up(5);
        app.clear_messages();
        assert!(app.messages.is_empty());
        assert_eq!(app.scroll_offset, 0);
        assert!(!app.user_scrolled);
    }

    #[test]
    fn test_insert_newline() {
        let mut app = make_app();
        app.input = "hello".to_string();
        app.cursor_pos = 5;
        app.insert_newline();
        assert_eq!(app.input, "hello\n");
        assert_eq!(app.cursor_pos, 6);
    }

    #[test]
    fn test_input_line_count() {
        let mut app = make_app();
        app.input = "one".to_string();
        assert_eq!(app.input_line_count(), 1);
        app.input = "one\ntwo\nthree".to_string();
        assert_eq!(app.input_line_count(), 3);
    }

    #[test]
    fn test_request_cancel() {
        let mut app = make_app();
        app.state = AppState::Idle;
        app.request_cancel();
        assert!(!app.cancel_requested); // Only works during streaming

        app.state = AppState::Streaming;
        app.request_cancel();
        assert!(app.cancel_requested);
    }

    #[test]
    fn test_auto_scroll_on_stream() {
        let mut app = make_app();
        app.state = AppState::Idle;
        app.scroll_up(5);
        // user_scrolled = true, so stream text should not reset offset
        app.handle_server_event(ServerEvent::StreamText {
            content: "hello".to_string(),
        });
        assert_eq!(app.scroll_offset, 5);

        // If user hasn't scrolled, offset stays 0
        app.user_scrolled = false;
        app.scroll_offset = 0;
        app.handle_server_event(ServerEvent::StreamText {
            content: " world".to_string(),
        });
        assert_eq!(app.scroll_offset, 0);
    }
}
