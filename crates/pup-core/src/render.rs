/// Common rendering utilities shared across backends.
///
/// Each backend has its own format constraints (Telegram HTML, Discord markdown,
/// etc.), but some transforms are useful everywhere.
/// Strip markdown formatting to produce plain text.
#[allow(clippy::too_many_lines)]
pub fn strip_markdown(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            // Code fences
            '`' => {
                // Count backticks
                let mut count = 1;
                while chars.peek() == Some(&'`') {
                    chars.next();
                    count += 1;
                }
                if count >= 3 {
                    // Fenced code block: skip language hint on same line
                    for c in chars.by_ref() {
                        if c == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                    // Copy content until closing fence
                    let mut fence_ticks = 0;
                    for c in chars.by_ref() {
                        if c == '`' {
                            fence_ticks += 1;
                            if fence_ticks >= count {
                                // Skip rest of closing fence line
                                for c in chars.by_ref() {
                                    if c == '\n' {
                                        break;
                                    }
                                }
                                break;
                            }
                        } else {
                            if fence_ticks > 0 {
                                // Not a closing fence, emit the backticks we skipped
                                for _ in 0..fence_ticks {
                                    out.push('`');
                                }
                                fence_ticks = 0;
                            }
                            out.push(c);
                        }
                    }
                } else {
                    // Inline code: copy content until matching backticks
                    let mut found = false;
                    let mut inner = String::new();
                    let mut close_count = 0;
                    for c in chars.by_ref() {
                        if c == '`' {
                            close_count += 1;
                            if close_count == count {
                                found = true;
                                break;
                            }
                        } else {
                            if close_count > 0 {
                                for _ in 0..close_count {
                                    inner.push('`');
                                }
                                close_count = 0;
                            }
                            inner.push(c);
                        }
                    }
                    if found {
                        out.push_str(&inner);
                    } else {
                        // Unmatched backticks, emit as-is
                        for _ in 0..count {
                            out.push('`');
                        }
                        out.push_str(&inner);
                    }
                }
            }
            // Bold/italic markers
            '*' | '_' => {
                // Skip emphasis markers
                while chars.peek() == Some(&ch) {
                    chars.next();
                }
            }
            // Links: [text](url) → text
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
                // Check for (url) following
                if chars.peek() == Some(&'(') {
                    chars.next();
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
                    }
                    out.push_str(&text);
                } else {
                    out.push('[');
                    out.push_str(&text);
                    out.push(']');
                }
            }
            // Headers: strip leading #
            '#' if out.is_empty() || out.ends_with('\n') => {
                while chars.peek() == Some(&'#') {
                    chars.next();
                }
                if chars.peek() == Some(&' ') {
                    chars.next();
                }
            }
            _ => out.push(ch),
        }
    }

    out
}

/// Truncate a string to a maximum number of characters, appending an ellipsis
/// if truncated. Tries to break at a paragraph or word boundary.
pub fn truncate(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_owned();
    }

    let truncated = &text[..max_chars.min(text.len())];

    // Try to break at paragraph boundary
    if let Some(pos) = truncated.rfind("\n\n")
        && pos > max_chars / 2 {
            return format!("{}…", &truncated[..pos]);
        }

    // Try to break at line boundary
    if let Some(pos) = truncated.rfind('\n')
        && pos > max_chars / 2 {
            return format!("{}…", &truncated[..pos]);
        }

    // Break at word boundary
    if let Some(pos) = truncated.rfind(' ')
        && pos > max_chars / 2 {
            return format!("{}…", &truncated[..pos]);
        }

    format!("{truncated}…")
}

/// Extract a one-line preview from content (for notifications, etc.).
pub fn one_line_preview(text: &str, max_len: usize) -> String {
    let first_line = text.lines().next().unwrap_or("");
    let stripped = strip_markdown(first_line);
    if stripped.len() <= max_len {
        stripped
    } else {
        format!("{}…", &stripped[..max_len.min(stripped.len())])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_markdown_bold() {
        assert_eq!(strip_markdown("**hello** world"), "hello world");
    }

    #[test]
    fn test_strip_markdown_link() {
        assert_eq!(strip_markdown("[click](http://example.com)"), "click");
    }

    #[test]
    fn test_strip_markdown_inline_code() {
        assert_eq!(strip_markdown("use `foo()` here"), "use foo() here");
    }

    #[test]
    fn test_strip_markdown_header() {
        assert_eq!(strip_markdown("## Hello"), "Hello");
    }

    #[test]
    fn test_truncate_short() {
        assert_eq!(truncate("hello", 100), "hello");
    }

    #[test]
    fn test_truncate_long() {
        let text = "a ".repeat(100);
        let result = truncate(&text, 50);
        assert!(result.len() <= 52); // 50 + ellipsis char
        assert!(result.ends_with('…'));
    }

    #[test]
    fn test_one_line_preview() {
        assert_eq!(
            one_line_preview("**Hello** world\nmore text", 50),
            "Hello world"
        );
    }
}
