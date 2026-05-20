use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::{App, AppState, MessageRole};
use crate::markdown;

pub fn render(frame: &mut Frame, app: &App) {
    let input_height = (app.input_line_count() as u16).clamp(1, 10) + 2; // +2 for border + prompt line

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),            // Status bar
            Constraint::Min(3),               // Messages
            Constraint::Length(input_height), // Input
        ])
        .split(frame.area());

    render_status_bar(frame, app, chunks[0]);
    render_messages(frame, app, chunks[1]);
    render_input(frame, app, chunks[2]);
}

fn render_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let state_str = match app.state {
        AppState::Connecting => "connecting...",
        AppState::Idle => "idle",
        AppState::Sending => "sending...",
        AppState::Streaming => "streaming...",
    };

    let model_str = app.model.as_deref().unwrap_or("default");
    let left = format!(" [{}·{}]", app.agent_type, model_str);

    let session_str = app
        .session_id
        .as_deref()
        .map(|s| {
            let short = if s.len() > 8 { &s[..8] } else { s };
            format!("[session:{}] ", short)
        })
        .unwrap_or_default();

    let right = format!("{}[{state_str}] ", session_str);
    let total = left.len() + right.len();
    let width = area.width as usize;
    let padding = if width > total {
        " ".repeat(width - total)
    } else {
        String::new()
    };

    let line = Line::from(vec![
        Span::styled(left, Style::default().fg(Color::Cyan)),
        Span::raw(padding),
        Span::styled(right, Style::default().fg(Color::DarkGray)),
    ]);

    let bar = Paragraph::new(line).style(Style::default().bg(Color::DarkGray).fg(Color::White));
    frame.render_widget(bar, area);
}

fn render_messages(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    let last_idx = app.messages.len().saturating_sub(1);

    for (i, msg) in app.messages.iter().enumerate() {
        let is_last = i == last_idx;

        let mut msg_lines = if msg.role == MessageRole::ToolCall {
            markdown::render_tool_call(&msg.content, msg.tool_status)
        } else {
            let content = if msg.role == MessageRole::Assistant && app.state == AppState::Streaming && is_last {
                format!("{}▌", msg.content)
            } else {
                msg.content.clone()
            };
            markdown::render_message(&content, msg.role, &app.agent_type)
        };

        lines.append(&mut msg_lines);
        lines.push(Line::raw(String::new()));
    }

    if lines.is_empty() {
        lines.push(Line::styled(
            "  Type a message and press Enter to start...".to_owned(),
            Style::default().fg(Color::DarkGray),
        ));
    }

    let visible_height = area.height as usize;
    let total_lines = lines.len();

    // Calculate scroll: scroll_offset=0 means at bottom
    let max_scroll = total_lines.saturating_sub(visible_height);
    let scroll = if app.scroll_offset == 0 {
        max_scroll as u16
    } else {
        max_scroll.saturating_sub(app.scroll_offset) as u16
    };

    let messages_widget = Paragraph::new(lines)
        .block(Block::default().borders(Borders::NONE))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));

    frame.render_widget(messages_widget, area);

    // Scroll indicator
    if app.user_scrolled {
        let indicator = Span::styled(
            " ↓ more ".to_owned(),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        );
        let indicator_area = Rect {
            x: area.x + area.width.saturating_sub(10),
            y: area.y + area.height.saturating_sub(1),
            width: 9,
            height: 1,
        };
        frame.render_widget(Paragraph::new(Line::from(indicator)), indicator_area);
    }
}

fn render_input(frame: &mut Frame, app: &App, area: Rect) {
    let line_count = app.input_line_count();
    let prompt = if line_count > 1 {
        format!("[{} lines] > ", line_count)
    } else {
        "> ".to_owned()
    };

    let display_text = format!("{}{}", prompt, app.input);
    let input = Paragraph::new(display_text.as_str())
        .block(Block::default().borders(Borders::TOP))
        .style(Style::default().fg(Color::White))
        .wrap(Wrap { trim: false });
    frame.render_widget(input, area);

    // Cursor positioning (simplified: end of current line)
    let prompt_len = prompt.len() as u16;
    let input_before_cursor = &app.input[..app.cursor_pos];
    let last_line_len = input_before_cursor.rsplit('\n').next().unwrap_or("").len() as u16;
    let cursor_line = input_before_cursor.matches('\n').count() as u16;

    let cursor_x = area.x + prompt_len + last_line_len;
    let cursor_y = area.y + 1 + cursor_line; // +1 for top border
    frame.set_cursor_position((
        cursor_x.min(area.x + area.width - 1),
        cursor_y.min(area.y + area.height - 1),
    ));
}
