use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use crate::app::MessageRole;
use crate::client::ToolCallStatus;

use std::sync::LazyLock;

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

/// Render a chat message with markdown formatting into ratatui Lines.
pub fn render_message(content: &str, role: MessageRole, agent_name: &str) -> Vec<Line<'static>> {
    // If system error message, render all red
    if role == MessageRole::System && content.starts_with("[Error]") {
        return vec![Line::from(vec![
            Span::styled("[system] ".to_owned(), Style::default().fg(Color::Red)),
            Span::styled(
                content.to_owned(),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ])];
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut code_lines: Vec<String> = Vec::new();
    let mut is_first_line = true;

    for raw_line in content.lines() {
        // Code block fence detection
        if raw_line.trim_start().starts_with("```") {
            if !in_code_block {
                in_code_block = true;
                code_lang = raw_line.trim_start().trim_start_matches('`').trim().to_string();
                if !code_lang.is_empty() {
                    lines.push(Line::from(Span::styled(
                        format!(" [{}] ", code_lang),
                        Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
                    )));
                }
                code_lines.clear();
            } else {
                // Closing fence - flush code block with syntax highlighting
                let code_text = code_lines.join("\n") + "\n";
                let highlighted = highlight_code(&code_text, &code_lang);
                lines.extend(highlighted);
                in_code_block = false;
                code_lang.clear();
                code_lines.clear();
            }
            continue;
        }

        if in_code_block {
            code_lines.push(raw_line.to_string());
            continue;
        }

        // Normal line - parse inline markdown
        let spans = parse_inline_markdown(raw_line);

        // Add sender prefix to first line only
        if is_first_line {
            let mut first_spans = get_prefix_spans(role, agent_name);
            first_spans.extend(spans);
            lines.push(Line::from(first_spans));
            is_first_line = false;
        } else {
            lines.push(Line::from(spans));
        }
    }

    // Handle unclosed code block
    if in_code_block {
        let code_text = code_lines.join("\n") + "\n";
        let highlighted = highlight_code(&code_text, &code_lang);
        lines.extend(highlighted);
    }

    // Empty content
    if lines.is_empty() {
        let prefix = get_prefix_spans(role, agent_name);
        lines.push(Line::from(prefix));
    }

    lines
}

fn get_prefix_spans(role: MessageRole, agent_name: &str) -> Vec<Span<'static>> {
    match role {
        MessageRole::User => vec![Span::styled(
            "You: ".to_owned(),
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        )],
        MessageRole::Assistant => vec![Span::styled(
            format!("{}: ", agent_name),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )],
        MessageRole::System => vec![Span::styled("[system] ".to_owned(), Style::default().fg(Color::Red))],
        MessageRole::ToolCall => vec![],
    }
}

/// Parse inline markdown: **bold**, *italic*, `code`
fn parse_inline_markdown(text: &str) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut chars = text.chars().peekable();
    let mut current = String::new();
    let base_style = Style::default();

    while let Some(ch) = chars.next() {
        match ch {
            '`' => {
                // Inline code
                if !current.is_empty() {
                    spans.push(Span::styled(current.clone(), base_style));
                    current.clear();
                }
                let mut code = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '`' {
                        closed = true;
                        break;
                    }
                    code.push(c);
                }
                if closed {
                    spans.push(Span::styled(
                        code,
                        Style::default().fg(Color::White).bg(Color::DarkGray),
                    ));
                } else {
                    current.push('`');
                    current.push_str(&code);
                }
            }
            '*' => {
                // Check for ** (bold) or * (italic)
                let is_double = chars.peek().map(|c| *c == '*').unwrap_or(false);
                if is_double {
                    chars.next(); // consume second *
                    if !current.is_empty() {
                        spans.push(Span::styled(current.clone(), base_style));
                        current.clear();
                    }
                    let mut bold_text = String::new();
                    let mut closed = false;
                    while let Some(c) = chars.next() {
                        if c == '*' && chars.peek().map(|c| *c == '*').unwrap_or(false) {
                            chars.next();
                            closed = true;
                            break;
                        }
                        bold_text.push(c);
                    }
                    if closed {
                        spans.push(Span::styled(bold_text, Style::default().add_modifier(Modifier::BOLD)));
                    } else {
                        current.push_str("**");
                        current.push_str(&bold_text);
                    }
                } else {
                    // Single * - italic
                    if !current.is_empty() {
                        spans.push(Span::styled(current.clone(), base_style));
                        current.clear();
                    }
                    let mut italic_text = String::new();
                    let mut closed = false;
                    for c in chars.by_ref() {
                        if c == '*' {
                            closed = true;
                            break;
                        }
                        italic_text.push(c);
                    }
                    if closed {
                        spans.push(Span::styled(
                            italic_text,
                            Style::default().add_modifier(Modifier::ITALIC),
                        ));
                    } else {
                        current.push('*');
                        current.push_str(&italic_text);
                    }
                }
            }
            _ => {
                current.push(ch);
            }
        }
    }

    if !current.is_empty() {
        spans.push(Span::styled(current, base_style));
    }

    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }

    spans
}

fn highlight_code(code: &str, lang: &str) -> Vec<Line<'static>> {
    let syntax = SYNTAX_SET
        .find_syntax_by_token(lang)
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
    let theme = &THEME_SET.themes["base16-ocean.dark"];

    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut result: Vec<Line<'static>> = Vec::new();

    for line in LinesWithEndings::from(code) {
        let spans: Vec<Span<'static>> = match highlighter.highlight_line(line, &SYNTAX_SET) {
            Ok(ranges) => ranges
                .into_iter()
                .map(|(style, text)| {
                    let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
                    Span::styled(
                        text.trim_end_matches('\n').to_string(),
                        Style::default().fg(fg).bg(Color::Rgb(40, 44, 52)),
                    )
                })
                .collect(),
            Err(_) => vec![Span::styled(
                line.trim_end_matches('\n').to_owned(),
                Style::default().fg(Color::White).bg(Color::Rgb(40, 44, 52)),
            )],
        };

        let mut padded = vec![Span::styled(
            "  ".to_owned(),
            Style::default().bg(Color::Rgb(40, 44, 52)),
        )];
        padded.extend(spans);
        result.push(Line::from(padded));
    }

    result
}

pub fn render_tool_call(content: &str, status: Option<ToolCallStatus>) -> Vec<Line<'static>> {
    let (icon, icon_color) = match status {
        Some(ToolCallStatus::Completed) => ("✓", Color::Green),
        Some(ToolCallStatus::Error) => ("✗", Color::Red),
        _ => ("⏳", Color::Yellow),
    };

    vec![Line::from(vec![
        Span::styled(
            format!(" {icon} "),
            Style::default().fg(icon_color),
        ),
        Span::styled(
            format!("[{content}]"),
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
        ),
    ])]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_text_user() {
        let lines = render_message("hello world", MessageRole::User, "Claude");
        assert_eq!(lines.len(), 1);
        // First span should be "You: " prefix
        let spans = &lines[0].spans;
        assert!(spans.len() >= 2);
        assert_eq!(spans[0].content.as_ref(), "You: ");
    }

    #[test]
    fn test_plain_text_assistant() {
        let lines = render_message("hi there", MessageRole::Assistant, "Claude");
        let spans = &lines[0].spans;
        assert_eq!(spans[0].content.as_ref(), "Claude: ");
    }

    #[test]
    fn test_error_message() {
        let lines = render_message("[Error] something broke", MessageRole::System, "Claude");
        assert_eq!(lines.len(), 1);
        let spans = &lines[0].spans;
        assert_eq!(spans[0].content.as_ref(), "[system] ");
        assert_eq!(spans[1].content.as_ref(), "[Error] something broke");
    }

    #[test]
    fn test_bold_inline() {
        let lines = render_message("this is **bold** text", MessageRole::User, "Claude");
        let spans = &lines[0].spans;
        // You: | this is | bold | text
        assert!(spans.iter().any(|s| s.content.as_ref() == "bold"));
    }

    #[test]
    fn test_italic_inline() {
        let lines = render_message("this is *italic* text", MessageRole::User, "Claude");
        let spans = &lines[0].spans;
        assert!(spans.iter().any(|s| s.content.as_ref() == "italic"));
    }

    #[test]
    fn test_inline_code() {
        let lines = render_message("run `cargo test` now", MessageRole::User, "Claude");
        let spans = &lines[0].spans;
        assert!(spans.iter().any(|s| s.content.as_ref() == "cargo test"));
    }

    #[test]
    fn test_code_block() {
        let content = "here:\n```rust\nfn main() {}\n```\ndone";
        let lines = render_message(content, MessageRole::Assistant, "Claude");
        // Should have: prefix line, [rust] header, code line, "done" line
        assert!(lines.len() >= 4);
    }

    #[test]
    fn test_multiline() {
        let lines = render_message("line1\nline2\nline3", MessageRole::User, "Claude");
        assert_eq!(lines.len(), 3);
        // Only first line has prefix
        assert_eq!(lines[0].spans[0].content.as_ref(), "You: ");
    }
}
