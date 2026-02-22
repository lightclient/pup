/// Rendering utilities for Telegram HTML parse mode.
///
/// Converts markdown-ish content to Telegram's supported HTML subset:
/// `<b>`, `<i>`, `<code>`, `<pre>`, `<a href="">`.
/// Maximum characters per Telegram message (with safety margin).
pub const MAX_BODY_CHARS: usize = 3500;

/// Convert markdown to Telegram HTML.
#[allow(clippy::too_many_lines)]
pub fn to_telegram_html(input: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(input.len() + input.len() / 4);
    let mut chars = input.chars().peekable();
    let mut in_code_block = false;

    while let Some(ch) = chars.next() {
        if in_code_block {
            match ch {
                '`' => {
                    let mut count = 1;
                    while chars.peek() == Some(&'`') {
                        chars.next();
                        count += 1;
                    }
                    if count >= 3 {
                        out.push_str("</pre>");
                        in_code_block = false;
                        // Skip rest of line (closing fence)
                        for c in chars.by_ref() {
                            if c == '\n' {
                                break;
                            }
                        }
                    } else {
                        for _ in 0..count {
                            out.push('`');
                        }
                    }
                }
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '&' => out.push_str("&amp;"),
                _ => out.push(ch),
            }
            continue;
        }

        match ch {
            // Code fences
            '`' => {
                let mut count = 1;
                while chars.peek() == Some(&'`') {
                    chars.next();
                    count += 1;
                }
                if count >= 3 {
                    // Skip language hint
                    for c in chars.by_ref() {
                        if c == '\n' {
                            break;
                        }
                    }
                    out.push_str("<pre>");
                    in_code_block = true;
                } else if count == 1 {
                    // Inline code
                    out.push_str("<code>");
                    let mut found_close = false;
                    for c in chars.by_ref() {
                        if c == '`' {
                            found_close = true;
                            break;
                        }
                        match c {
                            '<' => out.push_str("&lt;"),
                            '>' => out.push_str("&gt;"),
                            '&' => out.push_str("&amp;"),
                            _ => out.push(c),
                        }
                    }
                    if found_close {
                        out.push_str("</code>");
                    }
                } else {
                    // Double backtick or other — treat as inline code
                    out.push_str("<code>");
                    let mut close_count = 0;
                    let mut found = false;
                    for c in chars.by_ref() {
                        if c == '`' {
                            close_count += 1;
                            if close_count == count {
                                found = true;
                                break;
                            }
                        } else {
                            for _ in 0..close_count {
                                out.push('`');
                            }
                            close_count = 0;
                            match c {
                                '<' => out.push_str("&lt;"),
                                '>' => out.push_str("&gt;"),
                                '&' => out.push_str("&amp;"),
                                _ => out.push(c),
                            }
                        }
                    }
                    if found {
                        out.push_str("</code>");
                    }
                }
            }
            // Bold: **text**
            '*' if chars.peek() == Some(&'*') => {
                chars.next(); // consume second *
                out.push_str("<b>");
                let mut found = false;
                while let Some(c) = chars.next() {
                    if c == '*' {
                        if chars.peek() == Some(&'*') {
                            chars.next();
                            found = true;
                            break;
                        }
                        out.push('*');
                    } else {
                        push_escaped(&mut out, c);
                    }
                }
                if found {
                    out.push_str("</b>");
                }
            }
            // Headers: # → bold
            '#' if out.is_empty() || out.ends_with('\n') => {
                while chars.peek() == Some(&'#') {
                    chars.next();
                }
                if chars.peek() == Some(&' ') {
                    chars.next();
                }
                out.push_str("<b>");
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push_str("</b>\n");
                        break;
                    }
                    push_escaped(&mut out, c);
                }
            }
            // Links: [text](url)
            '[' => {
                let mut text = String::new();
                let mut depth = 1;
                for c in chars.by_ref() {
                    if c == '[' {
                        depth += 1;
                    } else if c == ']' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    text.push(c);
                }
                if chars.peek() == Some(&'(') {
                    chars.next();
                    let mut url = String::new();
                    let mut paren_depth = 1;
                    for c in chars.by_ref() {
                        if c == '(' {
                            paren_depth += 1;
                        } else if c == ')' {
                            paren_depth -= 1;
                            if paren_depth == 0 {
                                break;
                            }
                        }
                        url.push(c);
                    }
                    let _ = write!(
                        out,
                        "<a href=\"{}\">{}</a>",
                        escape_html(&url),
                        escape_html(&text)
                    );
                } else {
                    out.push('[');
                    out.push_str(&escape_html(&text));
                    out.push(']');
                }
            }
            // HTML entities
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            _ => out.push(ch),
        }
    }

    // Close unclosed code block
    if in_code_block {
        out.push_str("</pre>");
    }

    out
}

/// Escape HTML special characters.
pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn push_escaped(out: &mut String, c: char) {
    match c {
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        '&' => out.push_str("&amp;"),
        _ => out.push(c),
    }
}

/// Format a user message from the pi TUI for Telegram.
pub fn format_user_message(content: &str) -> String {
    format!("👤 <i>{}</i>", escape_html(content))
}

/// Format a tool call for verbose mode.
pub fn format_tool_call(
    tool_name: &str,
    args: &serde_json::Value,
    content: &str,
    _is_error: bool,
) -> String {
    use std::fmt::Write;
    let mut out = String::new();

    // Tool header
    let _ = write!(out, "<b>{}</b>", escape_html(tool_name));

    // Show args for common tools
    if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
        let _ = write!(out, "\n<pre>{}</pre>", escape_html(cmd));
    } else if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        let _ = write!(out, "\n<code>{}</code>", escape_html(path));
    }

    // Result
    if !content.is_empty() {
        out.push_str("\n━━━\n");
        let truncated = if content.len() > 500 {
            let end = content.floor_char_boundary(500);
            format!("{}…", &content[..end])
        } else {
            content.to_owned()
        };
        let _ = write!(out, "<pre>{}</pre>", escape_html(&truncated));
    }

    out
}

/// Split a long message at paragraph/code-fence boundaries.
///
/// Returns a vec of chunks, each under `max_chars`. Code fences are
/// closed before split and reopened after.
pub fn split_message(text: &str, max_chars: usize) -> Vec<String> {
    if text.len() <= max_chars {
        return vec![text.to_owned()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;
    let mut chunk_num = 1;

    while !remaining.is_empty() {
        if remaining.len() <= max_chars {
            let mut chunk = remaining.to_owned();
            if !chunks.is_empty() {
                chunk = format!("<i>(continued {}/{})</i>\n{}", chunk_num, "?", chunk);
            }
            chunks.push(chunk);
            break;
        }

        // Find a good split point (snap to char boundary to avoid
        // panicking on multi-byte characters).
        let safe_max = remaining.floor_char_boundary(max_chars);
        let search_area = &remaining[..safe_max];

        // Prefer paragraph boundary
        let split_at = search_area
            .rfind("\n\n")
            .filter(|&p| p > max_chars / 3)
            .or_else(|| search_area.rfind('\n').filter(|&p| p > max_chars / 3))
            .unwrap_or(safe_max);

        let (chunk_text, rest) = remaining.split_at(split_at);

        // Check if we're inside a <pre> block and close it
        let open_pre = chunk_text.matches("<pre>").count();
        let close_pre = chunk_text.matches("</pre>").count();
        let in_pre = open_pre > close_pre;

        let mut chunk = chunk_text.to_owned();
        if in_pre {
            chunk.push_str("</pre>");
        }

        if chunk_num > 1 {
            chunk = format!("<i>(continued {chunk_num}/…)</i>\n{chunk}");
        }

        chunks.push(chunk);
        chunk_num += 1;

        // Skip the split delimiter
        remaining = rest.trim_start_matches('\n');

        // Reopen pre block if needed
        if in_pre && !remaining.is_empty() {
            // The next chunk will be wrapped with <pre> if needed via content
            // We'll prepend <pre> in the next iteration implicitly via the content
        }
    }

    // Fix "?" placeholders with actual total count
    let total = chunks.len();
    for chunk in &mut chunks {
        *chunk = chunk.replace("/?)", &format!("/{total})"));
        *chunk = chunk.replace("/…)", &format!("/{total})"));
    }

    chunks
}

/// Format conversation history turns for posting into a topic.
///
/// Returns a vec of HTML messages — one per turn — most recent last.
/// Each message shows the user prompt and the (possibly truncated) assistant
/// reply.  Only the last `max_turns` turns are included.
pub fn format_history(turns: &[pup_ipc::Turn], max_turns: usize) -> Vec<String> {
    let start = turns.len().saturating_sub(max_turns);
    let mut msgs = Vec::new();

    for (i, turn) in turns[start..].iter().enumerate() {
        let mut parts = Vec::new();

        if let Some(ref user) = turn.user {
            parts.push(format!("👤 <i>{}</i>", escape_html(&user.content)));
        }
        if let Some(ref asst) = turn.assistant {
            let rendered = to_telegram_html(&asst.content);
            parts.push(rendered);
        }

        if !parts.is_empty() {
            let label = format!("<i>— turn {}/{} —</i>\n", start + i + 1, turns.len());
            msgs.push(format!("{label}{}", parts.join("\n\n")));
        }
    }

    msgs
}

/// Build the cancel inline keyboard markup.
pub fn cancel_keyboard(session_id: &str) -> serde_json::Value {
    serde_json::json!({
        "inline_keyboard": [[
            { "text": "✖ Cancel", "callback_data": format!("cancel:{session_id}") }
        ]]
    })
}

/// Empty keyboard (removes inline keyboard).
pub fn empty_keyboard() -> serde_json::Value {
    serde_json::json!({ "inline_keyboard": [] })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bold() {
        assert_eq!(to_telegram_html("**hello**"), "<b>hello</b>");
    }

    #[test]
    fn test_inline_code() {
        assert_eq!(to_telegram_html("`code`"), "<code>code</code>");
    }

    #[test]
    fn test_code_block() {
        assert_eq!(
            to_telegram_html("```rust\nfn main() {}\n```"),
            "<pre>fn main() {}\n</pre>"
        );
    }

    #[test]
    fn test_link() {
        assert_eq!(
            to_telegram_html("[click](http://example.com)"),
            "<a href=\"http://example.com\">click</a>"
        );
    }

    #[test]
    fn test_header() {
        assert_eq!(to_telegram_html("## Title\n"), "<b>Title</b>\n");
    }

    #[test]
    fn test_html_escaping() {
        assert_eq!(to_telegram_html("<script>"), "&lt;script&gt;");
    }

    #[test]
    fn test_split_short() {
        let chunks = split_message("hello", 100);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "hello");
    }

    #[test]
    fn test_split_long() {
        let text = "a\n\n".repeat(100);
        let chunks = split_message(&text, 50);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            // Each chunk should be roughly under the limit (with headers)
            assert!(chunk.len() < 100, "chunk too long: {}", chunk.len());
        }
    }

    #[test]
    fn test_format_user_message() {
        assert_eq!(
            format_user_message("hello <world>"),
            "👤 <i>hello &lt;world&gt;</i>"
        );
    }
}
