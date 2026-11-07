//! # highlighter
//! Lightweight syntax highlighter tokenizer for closed code blocks.
//! Fixes audit issues:
//! - U8: Stateful cross-line highlighting (`/* ... */` block comments, Python `"""`/`'''` triple-quoted strings,
//!   JS/TS `` `...` `` template strings);
//! - U5 & F9: Removes dead per-token `col_start`/`col_width` fields while exposing `total_cols` for horizontal
//!   scroll width calculation in `CodeBlockView`.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightToken {
    pub text: String,
    pub color_rgb: [u8; 3],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightLine {
    pub tokens: Vec<HighlightToken>,
    /// Total visual column width of the line (consumed to compute CodeBlock horizontal scroll width).
    pub total_cols: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MultilineState {
    Normal,
    BlockComment,
    TripleDoubleQuote,
    TripleSingleQuote,
    BacktickTemplate,
}

const RUST_KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn",
    "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
    "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe",
    "use", "where", "while", "async", "await", "dyn",
];

const PYTHON_KEYWORDS: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
    "elif", "else", "except", "False", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "True",
    "try", "while", "with", "yield",
];

const JS_TS_KEYWORDS: &[&str] = &[
    "const", "let", "var", "function", "return", "if", "else", "for", "while", "switch",
    "case", "break", "continue", "default", "import", "export", "from", "class", "extends",
    "new", "this", "super", "try", "catch", "finally", "throw", "async", "await", "typeof",
    "instanceof", "interface", "type", "implements", "readonly", "public", "private",
];

const COMMON_TYPES: &[&str] = &[
    "bool", "char", "str", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32",
    "i64", "i128", "isize", "f32", "f64", "String", "Vec", "Option", "Result", "Some", "None",
    "Ok", "Err", "Box", "Rc", "Arc", "int", "float", "list", "dict", "set", "tuple",
    "number", "string", "boolean", "void", "any", "unknown", "never",
];

/// Expand `\t` to spaces aligned to 4-column tab stops.
pub fn expand_tabs(line: &str, tab_size: usize) -> String {
    let tab_size = tab_size.max(1);
    if !line.contains('\t') {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len() + 8);
    let mut col = 0usize;
    for ch in line.chars() {
        if ch == '\t' {
            let spaces = tab_size - (col % tab_size);
            for _ in 0..spaces {
                out.push(' ');
            }
            col += spaces;
        } else {
            out.push(ch);
            col += char_visual_cols(ch);
        }
    }
    out
}

#[inline]
pub fn char_visual_cols(ch: char) -> usize {
    if ch.is_ascii() {
        1
    } else if ('\u{1100}'..='\u{115F}').contains(&ch)
        || ('\u{2E80}'..='\u{A4CF}').contains(&ch)
        || ('\u{AC00}'..='\u{D7A3}').contains(&ch)
        || ('\u{F900}'..='\u{FAFF}').contains(&ch)
        || ('\u{FE10}'..='\u{FE6F}').contains(&ch)
        || ('\u{FF00}'..='\u{FF60}').contains(&ch)
        || ('\u{FFE0}'..='\u{FFE6}').contains(&ch)
    {
        2
    } else {
        1
    }
}

#[inline]
pub fn str_visual_cols(s: &str) -> usize {
    s.chars().map(char_visual_cols).sum()
}

pub fn highlight_code(code: &str, lang: &str, dark_mode: bool) -> Vec<HighlightLine> {
    let lang = lang.trim().to_lowercase();
    let mut lines = Vec::new();

    let default_color = if dark_mode { [212, 212, 212] } else { [36, 41, 46] };
    let keyword_color = if dark_mode { [86, 156, 214] } else { [0, 92, 197] };
    let type_color = if dark_mode { [78, 201, 176] } else { [111, 66, 193] };
    let string_color = if dark_mode { [206, 145, 120] } else { [163, 21, 21] };
    let comment_color = if dark_mode { [106, 153, 85] } else { [106, 115, 125] };
    let number_color = if dark_mode { [181, 206, 168] } else { [0, 92, 197] };

    let supports_c_comments = matches!(
        lang.as_str(),
        "rust"
            | "rs"
            | "c"
            | "cpp"
            | "c++"
            | "js"
            | "javascript"
            | "ts"
            | "typescript"
            | "go"
            | "java"
            | "css"
            | ""
    );
    let supports_hash_comments = matches!(
        lang.as_str(),
        "python" | "py" | "sh" | "bash" | "yaml" | "yml" | "toml" | "ruby"
    );
    let supports_triple_quotes = matches!(lang.as_str(), "python" | "py" | "rust" | "rs" | "");
    let supports_backtick_strings = matches!(
        lang.as_str(),
        "js" | "javascript" | "ts" | "typescript" | "go"
    );

    let mut state = MultilineState::Normal;

    for raw_line in code.lines() {
        let expanded = expand_tabs(raw_line.trim_end_matches('\r'), 4);
        let chars: Vec<char> = expanded.chars().collect();
        let len = chars.len();
        let mut i = 0usize;
        let mut raw_tokens: Vec<(String, [u8; 3])> = Vec::new();

        while i < len {
            // 1. Continue active multiline state (Fix U8)
            match state {
                MultilineState::BlockComment => {
                    if let Some(rel_end) = find_seq(&chars[i..], &['*', '/']) {
                        let end = i + rel_end + 2;
                        let text: String = chars[i..end].iter().collect();
                        raw_tokens.push((text, comment_color));
                        i = end;
                        state = MultilineState::Normal;
                    } else {
                        let text: String = chars[i..].iter().collect();
                        raw_tokens.push((text, comment_color));
                        break;
                    }
                    continue;
                }
                MultilineState::TripleDoubleQuote => {
                    if let Some(rel_end) = find_seq(&chars[i..], &['"', '"', '"']) {
                        let end = i + rel_end + 3;
                        let text: String = chars[i..end].iter().collect();
                        raw_tokens.push((text, string_color));
                        i = end;
                        state = MultilineState::Normal;
                    } else {
                        let text: String = chars[i..].iter().collect();
                        raw_tokens.push((text, string_color));
                        break;
                    }
                    continue;
                }
                MultilineState::TripleSingleQuote => {
                    if let Some(rel_end) = find_seq(&chars[i..], &['\'', '\'', '\'']) {
                        let end = i + rel_end + 3;
                        let text: String = chars[i..end].iter().collect();
                        raw_tokens.push((text, string_color));
                        i = end;
                        state = MultilineState::Normal;
                    } else {
                        let text: String = chars[i..].iter().collect();
                        raw_tokens.push((text, string_color));
                        break;
                    }
                    continue;
                }
                MultilineState::BacktickTemplate => {
                    let mut end = i;
                    let mut escaped = false;
                    let mut closed = false;
                    while end < len {
                        if escaped {
                            escaped = false;
                        } else if chars[end] == '\\' {
                            escaped = true;
                        } else if chars[end] == '`' {
                            end += 1;
                            closed = true;
                            break;
                        }
                        end += 1;
                    }
                    let text: String = chars[i..end].iter().collect();
                    raw_tokens.push((text, string_color));
                    i = end;
                    if closed {
                        state = MultilineState::Normal;
                    }
                    continue;
                }
                MultilineState::Normal => {}
            }

            // 2. Check for `/* ... */` block comment start (Fix U8)
            if supports_c_comments && i + 1 < len && chars[i] == '/' && chars[i + 1] == '*' {
                if let Some(rel_end) = find_seq(&chars[i + 2..], &['*', '/']) {
                    let end = i + 2 + rel_end + 2;
                    let text: String = chars[i..end].iter().collect();
                    raw_tokens.push((text, comment_color));
                    i = end;
                } else {
                    let text: String = chars[i..].iter().collect();
                    raw_tokens.push((text, comment_color));
                    state = MultilineState::BlockComment;
                    break;
                }
                continue;
            }

            // 3. Single-line comment `//` or `#`
            let is_slash_comment =
                supports_c_comments && i + 1 < len && chars[i] == '/' && chars[i + 1] == '/';
            let is_hash_comment = supports_hash_comments && chars[i] == '#';

            if is_slash_comment || is_hash_comment {
                let text: String = chars[i..].iter().collect();
                raw_tokens.push((text, comment_color));
                break;
            }

            // 4. Triple-quoted strings `"""` or `'''` (Fix U8)
            if supports_triple_quotes && i + 2 < len {
                if chars[i] == '"' && chars[i + 1] == '"' && chars[i + 2] == '"' {
                    if let Some(rel_end) = find_seq(&chars[i + 3..], &['"', '"', '"']) {
                        let end = i + 3 + rel_end + 3;
                        let text: String = chars[i..end].iter().collect();
                        raw_tokens.push((text, string_color));
                        i = end;
                    } else {
                        let text: String = chars[i..].iter().collect();
                        raw_tokens.push((text, string_color));
                        state = MultilineState::TripleDoubleQuote;
                        break;
                    }
                    continue;
                }
                if chars[i] == '\'' && chars[i + 1] == '\'' && chars[i + 2] == '\'' {
                    if let Some(rel_end) = find_seq(&chars[i + 3..], &['\'', '\'', '\'']) {
                        let end = i + 3 + rel_end + 3;
                        let text: String = chars[i..end].iter().collect();
                        raw_tokens.push((text, string_color));
                        i = end;
                    } else {
                        let text: String = chars[i..].iter().collect();
                        raw_tokens.push((text, string_color));
                        state = MultilineState::TripleSingleQuote;
                        break;
                    }
                    continue;
                }
            }

            // 5. JS/TS/Go Backtick template literal `` `...` `` (Fix U8)
            if supports_backtick_strings && chars[i] == '`' {
                let mut end = i + 1;
                let mut escaped = false;
                let mut closed = false;
                while end < len {
                    if escaped {
                        escaped = false;
                    } else if chars[end] == '\\' {
                        escaped = true;
                    } else if chars[end] == '`' {
                        end += 1;
                        closed = true;
                        break;
                    }
                    end += 1;
                }
                let text: String = chars[i..end].iter().collect();
                raw_tokens.push((text, string_color));
                i = end;
                if !closed {
                    state = MultilineState::BacktickTemplate;
                }
                continue;
            }

            // 6. Standard single-line string literal `"..."` or `'...'`
            if chars[i] == '"' || chars[i] == '\'' {
                let quote = chars[i];
                // Rust lifetime `'a` check: if `'ident` not followed by `'`, treat as identifier/type
                if quote == '\''
                    && (lang == "rust" || lang == "rs")
                    && i + 2 < len
                    && chars[i + 1].is_alphabetic()
                    && chars[i + 2] != '\''
                {
                    let mut end = i + 2;
                    while end < len && (chars[end].is_alphanumeric() || chars[end] == '_') {
                        end += 1;
                    }
                    let text: String = chars[i..end].iter().collect();
                    raw_tokens.push((text, keyword_color));
                    i = end;
                    continue;
                }

                let mut end = i + 1;
                let mut escaped = false;
                while end < len {
                    if escaped {
                        escaped = false;
                    } else if chars[end] == '\\' {
                        escaped = true;
                    } else if chars[end] == quote {
                        end += 1;
                        break;
                    }
                    end += 1;
                }
                let text: String = chars[i..end].iter().collect();
                raw_tokens.push((text, string_color));
                i = end;
                continue;
            }

            // 7. Identifier / keyword / type
            if chars[i].is_alphabetic() || chars[i] == '_' {
                let mut end = i + 1;
                while end < len && (chars[end].is_alphanumeric() || chars[end] == '_') {
                    end += 1;
                }
                let ident: String = chars[i..end].iter().collect();
                let color = if RUST_KEYWORDS.contains(&ident.as_str())
                    || PYTHON_KEYWORDS.contains(&ident.as_str())
                    || JS_TS_KEYWORDS.contains(&ident.as_str())
                {
                    keyword_color
                } else if COMMON_TYPES.contains(&ident.as_str())
                    || (ident
                        .chars()
                        .next()
                        .map(|c| c.is_uppercase())
                        .unwrap_or(false)
                        && ident.len() > 1)
                {
                    type_color
                } else {
                    default_color
                };
                raw_tokens.push((ident, color));
                i = end;
                continue;
            }

            // 8. Number literal
            if chars[i].is_ascii_digit() {
                let mut end = i + 1;
                while end < len && (chars[end].is_ascii_alphanumeric() || chars[end] == '.') {
                    end += 1;
                }
                let text: String = chars[i..end].iter().collect();
                raw_tokens.push((text, number_color));
                i = end;
                continue;
            }

            // 9. Whitespace & punctuation
            let mut end = i + 1;
            while end < len
                && !chars[end].is_alphanumeric()
                && chars[end] != '_'
                && chars[end] != '"'
                && chars[end] != '\''
                && chars[end] != '`'
                && !(chars[end] == '/'
                    && end + 1 < len
                    && (chars[end + 1] == '/' || chars[end + 1] == '*'))
                && chars[end] != '#'
            {
                end += 1;
            }
            let text: String = chars[i..end].iter().collect();
            raw_tokens.push((text, default_color));
            i = end;
        }

        // Coalesce adjacent tokens with the same color
        let mut coalesced: Vec<HighlightToken> = Vec::new();
        let mut col_cursor = 0usize;

        for (text, color_rgb) in raw_tokens {
            let width = str_visual_cols(&text);
            col_cursor += width;
            if let Some(last) = coalesced.last_mut() {
                if last.color_rgb == color_rgb {
                    last.text.push_str(&text);
                    continue;
                }
            }
            coalesced.push(HighlightToken { text, color_rgb });
        }

        if coalesced.is_empty() {
            coalesced.push(HighlightToken {
                text: " ".to_string(),
                color_rgb: default_color,
            });
            col_cursor = 1;
        }

        lines.push(HighlightLine {
            tokens: coalesced,
            total_cols: col_cursor,
        });
    }

    if lines.is_empty() {
        lines.push(HighlightLine {
            tokens: vec![HighlightToken {
                text: " ".to_string(),
                color_rgb: default_color,
            }],
            total_cols: 1,
        });
    }

    lines
}

fn find_seq(haystack: &[char], needle: &[char]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let limit = haystack.len() - needle.len();
    for idx in 0..=limit {
        if &haystack[idx..idx + needle.len()] == needle {
            return Some(idx);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_highlight_rust_code_and_coalescing() {
        let code = "fn main() {\n\tlet x: i32 = 42; // comment\n}";
        let lines = highlight_code(code, "rust", true);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].tokens[0].text, "fn");
        assert_eq!(lines[0].tokens[0].color_rgb, [86, 156, 214]);
        assert!(lines[1].tokens[0].text.starts_with("    "));
    }

    #[test]
    fn test_u8_multiline_block_comment_and_python_docstring() {
        let rust_code = "/* start comment\nstill in comment\nend */ let x = 1;";
        let r_lines = highlight_code(rust_code, "rust", true);
        // Line 1 (middle of block comment) must be colored with comment_color [106, 153, 85]
        assert_eq!(r_lines[1].tokens[0].color_rgb, [106, 153, 85]);

        let py_code = "def f():\n    \"\"\"doc line 1\n    doc line 2\"\"\"\n    return 1";
        let p_lines = highlight_code(py_code, "python", true);
        // Line 2 (`    doc line 2"""`) must be colored with string_color [206, 145, 120]
        assert_eq!(p_lines[2].tokens[0].color_rgb, [206, 145, 120]);
    }
}
