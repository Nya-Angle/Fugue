//! # markdown_parser
//! High-performance zero-regex-backtracking Markdown block classifier, $O(L)$ inline span tokenizer,
//! GFM table scanner with column alignment, Mermaid diagram parser, and lossless block copy serializer.
//! Faithfully ports `agent-block-copy.ts` and `stream-block-automaton.ts` semantics while fixing
//! audit issues L1, U2, U3, U4, U10, L4, L7, L8, L11, L12, L13, M3, M5, M7, F8.

use crate::highlighter::char_visual_cols;
use crate::latex_renderer::inline_latex_to_unicode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceChar {
    Backtick,
    Tilde,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FenceOpener {
    pub character: FenceChar,
    pub length: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListMarker {
    pub indent: usize,
    pub content_indent: usize,
    pub ordered: bool,
    pub marker_label: String,
    pub empty: bool,
    /// Optional GFM task checkbox (`Some(false)` for `[ ]`, `Some(true)` for `[x]`/`[X]`).
    pub task_checked: Option<bool>,
    /// Byte offset in the line where actual item content begins (after marker + optional `[ ] `).
    pub content_byte_offset: usize,
}

#[inline]
pub fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

#[inline]
pub fn leading_indent(line: &str) -> usize {
    line.bytes()
        .take_while(|&b| b == b' ' || b == b'\t')
        .count()
}

/// Matches `FENCE_OPENER_RE = /^[ \t]*(`{3,}|~{3,})([^\n]*)$/`
/// Also extracts the info string (`lang`).
/// Fix U10: Rejects single-line triple-backtick or triple-tilde blocks (e.g. ```` ```Content``` ````
/// or `~~~Content~~~`) so they are handled by `parse_single_line_triple_code`.
pub fn parse_fence_opener(line: &str) -> Option<(FenceOpener, String)> {
    let trimmed_start = line.trim_start_matches([' ', '\t']);
    let bytes = trimmed_start.as_bytes();
    if bytes.len() < 3 {
        return None;
    }
    let first = bytes[0];
    if first != b'`' && first != b'~' {
        return None;
    }
    let run_len = bytes.iter().take_while(|&&b| b == first).count();
    if run_len < 3 {
        return None;
    }
    let info = trimmed_start[run_len..].trim_end_matches('\r');
    // CommonMark spec: backtick fence info string cannot contain backticks
    if first == b'`' && info.contains('`') {
        return None;
    }
    // Fix U10: single-line tilde fence `~~~Content~~~` is also not a multi-line fence opener
    if first == b'~' && info.trim_end().ends_with("~~~") {
        return None;
    }
    let character = if first == b'`' {
        FenceChar::Backtick
    } else {
        FenceChar::Tilde
    };
    Some((
        FenceOpener {
            character,
            length: run_len,
        },
        info.trim().to_string(),
    ))
}

/// Detect single-line triple-backtick or triple-tilde block (```` ```Content``` ```` or `~~~Content~~~`).
pub fn parse_single_line_triple_code(line: &str) -> Option<String> {
    let s = line.trim();
    if s.len() >= 6 && !s.contains('\n') {
        if s.starts_with("```") && s.ends_with("```") {
            let inner = &s[3..s.len() - 3];
            if !inner.contains("```") {
                return Some(inner.trim().to_string());
            }
        } else if s.starts_with("~~~") && s.ends_with("~~~") {
            let inner = &s[3..s.len() - 3];
            if !inner.contains("~~~") {
                return Some(inner.trim().to_string());
            }
        }
    }
    None
}

/// Matches `FENCE_CLOSER_RE = /^[ \t]*(`{3,}|~{3,})[ \t]*$/`
pub fn is_fence_closer(line: &str, opener: FenceOpener) -> bool {
    let trimmed = line.trim_matches([' ', '\t', '\r']);
    if trimmed.len() < opener.length {
        return false;
    }
    let target = match opener.character {
        FenceChar::Backtick => b'`',
        FenceChar::Tilde => b'~',
    };
    trimmed.bytes().all(|b| b == target)
}

/// Matches `THEMATIC_BREAK_RE = /^[ \t]{0,3}(?:[-*_][ \t]*){3,}$/`
pub fn is_thematic_break(line: &str) -> bool {
    let indent = leading_indent(line);
    if indent > 3 {
        return false;
    }
    let rest = line[indent..].trim_end_matches([' ', '\t', '\r']);
    let mut marker: Option<u8> = None;
    let mut count = 0usize;
    for b in rest.bytes() {
        if b == b' ' || b == b'\t' {
            continue;
        }
        if b == b'-' || b == b'*' || b == b'_' {
            if let Some(m) = marker {
                if m != b {
                    return false;
                }
            } else {
                marker = Some(b);
            }
            count += 1;
        } else {
            return false;
        }
    }
    count >= 3
}

/// Matches `SETEXT_HEADING_RE = /^[ \t]{0,3}(?:={3,}|-{3,})[ \t]*$/`
/// Fix L13: Require at least 3 `=` or `-` characters so a lone `-` list starter is never misclassified
/// as a Setext heading underline.
/// Note: Merging `Paragraph + blank line + ===/---` into a Setext heading in `stream_automaton.rs`
/// is an intentional invariant required by the user's reference specification (`stream-block-automaton.ts` Test 3).
pub fn parse_setext_underline(line: &str) -> Option<u8> {
    let indent = leading_indent(line);
    if indent > 3 {
        return None;
    }
    let rest = line[indent..].trim_end_matches([' ', '\t', '\r']);
    if rest.len() < 3 {
        return None;
    }
    if rest.bytes().all(|b| b == b'=') {
        Some(1)
    } else if rest.bytes().all(|b| b == b'-') {
        Some(2)
    } else {
        None
    }
}

/// Matches `HEADING_RE = /^[ \t]{0,3}#{1,6}[ \t]/`
/// Fix L11: Only strip trailing `#` characters when preceded by whitespace (CommonMark spec),
/// preserving headings like `## C#` or `### F#`.
pub fn parse_atx_heading(line: &str) -> Option<(u8, &str)> {
    let indent = leading_indent(line);
    if indent > 3 {
        return None;
    }
    let rest = line[indent..].trim_end_matches('\r');
    let hashes = rest.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let after = &rest[hashes..];
    if after.is_empty() {
        return Some((hashes as u8, ""));
    }
    if after.starts_with(' ') || after.starts_with('\t') {
        let trimmed = after.trim();
        let without_closing_hashes = trimmed.trim_end_matches('#');
        let content = if without_closing_hashes.is_empty() {
            ""
        } else if without_closing_hashes.len() < trimmed.len()
            && without_closing_hashes.ends_with([' ', '\t'])
        {
            without_closing_hashes.trim_end()
        } else {
            trimmed
        };
        return Some((hashes as u8, content));
    }
    None
}

/// Matches `QUOTE_RE = /^[ \t]{0,3}>[ \t]?/`
pub fn is_quote_line(line: &str) -> bool {
    strip_quote_prefix(line).is_some()
}

pub fn strip_quote_prefix(line: &str) -> Option<&str> {
    let indent = leading_indent(line);
    if indent > 3 {
        return None;
    }
    let rest = line[indent..].trim_end_matches('\r');
    if let Some(after_gt) = rest.strip_prefix('>') {
        if let Some(stripped) = after_gt
            .strip_prefix(' ')
            .or_else(|| after_gt.strip_prefix('\t'))
        {
            Some(stripped)
        } else {
            Some(after_gt)
        }
    } else {
        None
    }
}

/// Matches `LIST_MARKER_RE = /^([ \t]{0,3})([-+*]|\d+[.)])(?:([ \t]+)(.*))?$/`
/// Fix L1 (P0): Use byte-level ASCII check `content_part.as_bytes()` instead of `&content_part[..3]`
/// so non-ASCII multi-byte UTF-8 characters (Cyrillic `- привет`, Emoji `- 😀`) NEVER panic!
pub fn parse_list_marker(line: &str) -> Option<ListMarker> {
    if is_thematic_break(line) {
        return None;
    }
    let line_clean = line.trim_end_matches('\r');
    let indent = leading_indent(line_clean);
    if indent > 3 {
        return None;
    }
    let rest = &line_clean[indent..];
    if rest.is_empty() {
        return None;
    }

    let bytes = rest.as_bytes();
    let (marker_width, ordered, marker_label) =
        if bytes[0] == b'-' || bytes[0] == b'+' || bytes[0] == b'*' {
            (1usize, false, "•".to_string())
        } else if bytes[0].is_ascii_digit() {
            let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
            if digits == 0 || digits > 9 || digits >= bytes.len() {
                return None;
            }
            let delim = bytes[digits];
            if delim != b'.' && delim != b')' {
                return None;
            }
            (digits + 1, true, rest[..digits + 1].to_string())
        } else {
            return None;
        };

    let after_marker = &rest[marker_width..];
    if after_marker.is_empty() {
        return Some(ListMarker {
            indent,
            content_indent: indent + marker_width + 1,
            ordered,
            marker_label,
            empty: true,
            task_checked: None,
            content_byte_offset: line_clean.len(),
        });
    }

    let ws_len = after_marker
        .bytes()
        .take_while(|&b| b == b' ' || b == b'\t')
        .count();
    if ws_len == 0 {
        return None;
    }

    let content_part = &after_marker[ws_len..];
    let cb = content_part.as_bytes();
    let mut task_checked = None;
    let mut extra_offset = 0usize;

    if cb.len() >= 3 && cb[0] == b'[' && cb[2] == b']' {
        let valid_after = cb.len() == 3 || cb[3] == b' ' || cb[3] == b'\t';
        if valid_after {
            if cb[1] == b' ' {
                task_checked = Some(false);
                extra_offset = if cb.len() > 3 { 4 } else { 3 };
            } else if cb[1] == b'x' || cb[1] == b'X' {
                task_checked = Some(true);
                extra_offset = if cb.len() > 3 { 4 } else { 3 };
            }
        }
    }

    let final_content = &content_part[extra_offset..];
    Some(ListMarker {
        indent,
        content_indent: indent + marker_width + ws_len,
        ordered,
        marker_label,
        empty: final_content.trim().is_empty() && task_checked.is_none(),
        task_checked,
        content_byte_offset: indent + marker_width + ws_len + extra_offset,
    })
}

/// Parse a GFM table separator line (`| :--- | :---: | ---: |`) and return column alignments:
/// `0` = Left, `1` = Center, `2` = Right.
pub fn parse_table_separator_alignments(line: &str) -> Option<Vec<u8>> {
    let trimmed = line.trim();
    if !trimmed.contains('|') {
        return None;
    }
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    let parts: Vec<&str> = inner.split('|').collect();
    if parts.is_empty() {
        return None;
    }
    let mut aligns = Vec::with_capacity(parts.len());
    for part in parts {
        let cell = part.trim();
        if cell.is_empty() {
            return None;
        }
        let left_colon = cell.starts_with(':');
        let right_colon = cell.ends_with(':');
        let dashes = cell.strip_prefix(':').unwrap_or(cell);
        let dashes = dashes.strip_suffix(':').unwrap_or(dashes);
        if dashes.len() < 3 || !dashes.bytes().all(|b| b == b'-') {
            return None;
        }
        let align = match (left_colon, right_colon) {
            (true, true) => 1,  // Center
            (false, true) => 2, // Right
            _ => 0,             // Left
        };
        aligns.push(align);
    }
    Some(aligns)
}

/// Matches `TABLE_SEPARATOR_RE = /^\s*\|?\s*:?-{3,}:?\s*(?:\|\s*:?-{3,}:?\s*)+\|?\s*$/`
#[inline]
pub fn is_table_separator(line: &str) -> bool {
    parse_table_separator_alignments(line).is_some()
}

/// Scan a single GFM table row into cells, handling escaped pipes `\|` exactly like `agent-block-copy.ts`.
pub fn scan_markdown_table_row(line: &str) -> Vec<String> {
    let trimmed_line = line.trim();
    let mut cells: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut trailing_backslashes = 0usize;
    let mut last_pipe_escaped = false;

    for ch in trimmed_line.chars() {
        if ch == '|' {
            last_pipe_escaped = trailing_backslashes % 2 == 1;
            if last_pipe_escaped {
                cell.pop();
                cell.push('|');
                trailing_backslashes = 0;
            } else {
                cells.push(cell.trim().to_string());
                cell.clear();
                trailing_backslashes = 0;
            }
            continue;
        }
        last_pipe_escaped = false;
        cell.push(ch);
        if ch == '\\' {
            trailing_backslashes += 1;
        } else {
            trailing_backslashes = 0;
        }
    }
    cells.push(cell.trim().to_string());

    if trimmed_line.starts_with('|') && !cells.is_empty() {
        cells.remove(0);
    }
    if trimmed_line.ends_with('|') && !last_pipe_escaped && !cells.is_empty() {
        cells.pop();
    }
    cells
}

/// Parse a full GFM table block into `(headers, alignments, rows)`.
/// Fix U3 & L12: Preserves column alignments (`0`=Left, `1`=Center, `2`=Right) and expands `col_count`
/// to the maximum column width across headers and all rows so extra cells are never truncated!
pub fn parse_gfm_table_with_aligns(source: &str) -> (Vec<String>, Vec<u8>, Vec<Vec<String>>) {
    let mut lines = source.lines().filter(|l| l.contains('|') && !is_blank(l));
    let header_line = match lines.next() {
        Some(h) => h,
        None => return (Vec::new(), Vec::new(), Vec::new()),
    };
    let mut headers = scan_markdown_table_row(header_line);
    let mut aligns = Vec::new();
    let mut rows = Vec::new();
    let mut max_cols = headers.len().max(1);

    for line in lines {
        if let Some(parsed_aligns) = parse_table_separator_alignments(line) {
            if aligns.is_empty() {
                aligns = parsed_aligns;
                max_cols = max_cols.max(aligns.len());
            }
            continue;
        }
        let row = scan_markdown_table_row(line);
        max_cols = max_cols.max(row.len());
        rows.push(row);
    }

    headers.resize(max_cols, String::new());
    aligns.resize(max_cols, 0);
    for row in &mut rows {
        row.resize(max_cols, String::new());
    }
    (headers, aligns, rows)
}

pub fn parse_gfm_table(source: &str) -> (Vec<String>, Vec<Vec<String>>) {
    let (h, _, r) = parse_gfm_table_with_aligns(source);
    (h, r)
}

/// Convert a GFM table block source into TSV string (matching `tableMarkdownToTsv` in `agent-block-copy.ts`).
pub fn table_markdown_to_tsv(source: &str) -> String {
    source
        .lines()
        .filter(|line| line.contains('|') && !is_table_separator(line))
        .map(|line| {
            scan_markdown_table_row(line)
                .into_iter()
                .map(|cell| strip_inline_markers(&cell))
                .collect::<Vec<_>>()
                .join("\t")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Convert a GFM table block source into space-aligned plain text (for "Copy as Plain Text", Fix L8).
pub fn table_markdown_to_plain_text(source: &str) -> String {
    let (headers, _, rows) = parse_gfm_table_with_aligns(source);
    if headers.is_empty() && rows.is_empty() {
        return String::new();
    }
    let clean_headers: Vec<String> = headers.iter().map(|h| strip_inline_markers(h)).collect();
    let clean_rows: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(|c| strip_inline_markers(c)).collect())
        .collect();

    let mut out = Vec::with_capacity(1 + clean_rows.len());
    out.push(clean_headers.join(" | "));
    for r in clean_rows {
        out.push(r.join(" | "));
    }
    out.join("\n")
}

/// Convert Markdown source into clean plain text for clipboard (`markdownToPlainText` in `agent-block-copy.ts`).
/// Fix L8: Tracks fenced code blocks (`in_fence`) and single-line triple code so code content
/// (such as `**kwargs`, `` `x` ``, `$var`) is NEVER corrupted by `strip_inline_markers`!
pub fn markdown_to_plain_text(markdown: &str) -> String {
    if let Some(single_code) = parse_single_line_triple_code(markdown) {
        return single_code;
    }

    let mut out_lines = Vec::new();
    let mut active_fence: Option<FenceOpener> = None;

    for raw_line in markdown.lines() {
        let line = raw_line.trim_end_matches('\r');

        if let Some(opener) = active_fence {
            if is_fence_closer(line, opener) {
                active_fence = None;
            } else {
                // Inside code fence: preserve raw code verbatim!
                out_lines.push(line.to_string());
            }
            continue;
        }

        if let Some((opener, _)) = parse_fence_opener(line) {
            active_fence = Some(opener);
            continue;
        }

        if let Some(single_code) = parse_single_line_triple_code(line) {
            out_lines.push(single_code);
            continue;
        }

        if is_thematic_break(line) {
            continue;
        }

        let mut s = line;
        if let Some((_, h_text)) = parse_atx_heading(s) {
            s = h_text;
        } else if let Some(q_text) = strip_quote_prefix(s) {
            s = q_text;
        } else if let Some(marker) = parse_list_marker(s) {
            s = &s[marker.content_byte_offset..];
        } else if parse_setext_underline(s).is_some() {
            continue;
        }
        out_lines.push(strip_inline_markers(s));
    }
    out_lines.join("\n").trim().to_string()
}

/// Strip inline formatting markers (`**`, `*`, `_`, `` ` ``, `~~`, `[label](url)`) for plain text export.
pub fn strip_inline_markers(text: &str) -> String {
    let spans = tokenize_inline_spans(text);
    let mut out = String::with_capacity(text.len());
    for span in spans {
        out.push_str(&span.text);
    }
    out
}

/// Inline rich-text token style flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineSpan {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub strikethrough: bool,
    pub math: bool,
    /// Optional raw LaTeX formula when `math == true` (used for inline PNG hydration, Fix U6).
    pub raw_math: String,
    /// Optional target URL for links/images/autolinks (Fix U4 & M5).
    pub link_url: String,
}

impl InlineSpan {
    pub fn plain(text: String) -> Self {
        Self {
            text,
            bold: false,
            italic: false,
            code: false,
            strikethrough: false,
            math: false,
            raw_math: String::new(),
            link_url: String::new(),
        }
    }
}

/// Single-pass $O(L)$ inline Markdown tokenizer supporting:
/// - `` `code` `` and ```` ```code``` ````
/// - `$$math$$`, `$math$`, `\(math\)`
/// - `**bold**` and `__bold__`
/// - `~~strikethrough~~`
/// - `*italic*` and word-boundary `_italic_` (Fix U4)
/// - `![alt](url)` images and `[label](url)` links (Fix U4 & M5)
/// - `<https://...>` / `<http://...>` / `<mailto:...>` autolinks (Fix U4 & M5)
/// - Failure memoization watermarks to guarantee strict $O(L)$ worst-case complexity (Fix F8).
pub fn tokenize_inline_spans(input: &str) -> Vec<InlineSpan> {
    let mut spans = Vec::new();
    let mut plain_buf = String::new();
    let chars: Vec<char> = input.chars().collect();
    let n = chars.len();
    let mut i = 0usize;

    // Failure flags (Fix F8): once a closing sequence is not found in the remainder of `chars`,
    // we never re-scan for that sequence again, preventing $O(L^2)$ degradation on pathological inputs.
    let mut no_more_double_dollar = false;
    let mut no_more_single_dollar = false;
    let mut no_more_triple_star = false;
    let mut no_more_double_star = false;
    let mut no_more_double_under = false;
    let mut no_more_double_tilde = false;
    let mut no_more_single_star = false;
    let mut no_more_single_under = false;

    let flush_plain = |buf: &mut String, out: &mut Vec<InlineSpan>| {
        if !buf.is_empty() {
            out.push(InlineSpan::plain(std::mem::take(buf)));
        }
    };

    while i < n {
        // 1. Escaped character `\*`, `\_`, etc. (except `\(` which is inline LaTeX math)
        if chars[i] == '\\' && i + 1 < n {
            let next = chars[i + 1];
            if next == '(' {
                if let Some(close_offset) = find_char_seq(&chars[i + 2..], &['\\', ')']) {
                    flush_plain(&mut plain_buf, &mut spans);
                    let raw_math: String = chars[i + 2..i + 2 + close_offset].iter().collect();
                    spans.push(InlineSpan {
                        text: inline_latex_to_unicode(&raw_math),
                        bold: false,
                        italic: true,
                        code: false,
                        strikethrough: false,
                        math: true,
                        raw_math,
                        link_url: String::new(),
                    });
                    i += 2 + close_offset + 2;
                    continue;
                }
            } else if matches!(next, '*' | '_' | '`' | '~' | '[' | ']' | '\\' | '$') {
                plain_buf.push(next);
                i += 2;
                continue;
            }
        }

        // 2. Inline code `` `...` `` or ```` ```...``` ````
        if chars[i] == '`' {
            let mut tick_count = 1usize;
            while i + tick_count < n && chars[i + tick_count] == '`' {
                tick_count += 1;
            }
            let pat: Vec<char> = vec!['`'; tick_count];
            if let Some(close_offset) = find_exact_backtick_run(&chars[i + tick_count..], tick_count)
                .or_else(|| find_char_seq(&chars[i + tick_count..], &pat))
            {
                flush_plain(&mut plain_buf, &mut spans);
                let code_str: String = chars[i + tick_count..i + tick_count + close_offset]
                    .iter()
                    .collect();
                let clean_code = if code_str.len() >= 2
                    && code_str.starts_with(' ')
                    && code_str.ends_with(' ')
                {
                    code_str[1..code_str.len() - 1].to_string()
                } else {
                    code_str
                };
                spans.push(InlineSpan {
                    text: clean_code,
                    bold: false,
                    italic: false,
                    code: true,
                    strikethrough: false,
                    math: false,
                    raw_math: String::new(),
                    link_url: String::new(),
                });
                i += tick_count + close_offset + tick_count;
                continue;
            } else {
                // Consume all backticks in this unmatched run as plain text
                for _ in 0..tick_count {
                    plain_buf.push('`');
                }
                i += tick_count;
                continue;
            }
        }

        // 3. Inline or single-line math `$$...$$` or `$...$`
        if chars[i] == '$' {
            if i + 1 < n && chars[i + 1] == '$' && !no_more_double_dollar {
                if let Some(close_offset) = find_char_seq(&chars[i + 2..], &['$', '$']) {
                    if close_offset > 0 {
                        flush_plain(&mut plain_buf, &mut spans);
                        let raw_math: String = chars[i + 2..i + 2 + close_offset].iter().collect();
                        spans.push(InlineSpan {
                            text: inline_latex_to_unicode(&raw_math),
                            bold: false,
                            italic: true,
                            code: false,
                            strikethrough: false,
                            math: true,
                            raw_math,
                            link_url: String::new(),
                        });
                        i += 2 + close_offset + 2;
                        continue;
                    }
                } else {
                    no_more_double_dollar = true;
                }
            } else if i + 1 < n && !chars[i + 1].is_whitespace() && !no_more_single_dollar {
                if let Some(close_offset) = find_char_seq(&chars[i + 1..], &['$']) {
                    if close_offset > 0 && !chars[i + close_offset].is_whitespace() {
                        flush_plain(&mut plain_buf, &mut spans);
                        let raw_math: String = chars[i + 1..i + 1 + close_offset].iter().collect();
                        spans.push(InlineSpan {
                            text: inline_latex_to_unicode(&raw_math),
                            bold: false,
                            italic: true,
                            code: false,
                            strikethrough: false,
                            math: true,
                            raw_math,
                            link_url: String::new(),
                        });
                        i += 1 + close_offset + 1;
                        continue;
                    }
                } else {
                    no_more_single_dollar = true;
                }
            }
        }

        // 4. Bold + Italic `***...***`
        if i + 2 < n
            && chars[i] == '*'
            && chars[i + 1] == '*'
            && chars[i + 2] == '*'
            && !no_more_triple_star
        {
            if let Some(close_offset) = find_char_seq(&chars[i + 3..], &['*', '*', '*']) {
                if close_offset > 0 {
                    flush_plain(&mut plain_buf, &mut spans);
                    let inner: String = chars[i + 3..i + 3 + close_offset].iter().collect();
                    spans.push(InlineSpan {
                        text: inner,
                        bold: true,
                        italic: true,
                        code: false,
                        strikethrough: false,
                        math: false,
                        raw_math: String::new(),
                        link_url: String::new(),
                    });
                    i += 3 + close_offset + 3;
                    continue;
                }
            } else {
                no_more_triple_star = true;
            }
        }

        // 5. Bold `**...**` or `__...__`
        if i + 1 < n && chars[i] == '*' && chars[i + 1] == '*' && !no_more_double_star {
            if let Some(close_offset) = find_char_seq(&chars[i + 2..], &['*', '*']) {
                if close_offset > 0 {
                    flush_plain(&mut plain_buf, &mut spans);
                    let inner: String = chars[i + 2..i + 2 + close_offset].iter().collect();
                    for mut sub in tokenize_inline_spans(&inner) {
                        sub.bold = true;
                        spans.push(sub);
                    }
                    i += 2 + close_offset + 2;
                    continue;
                }
            } else {
                no_more_double_star = true;
            }
        }

        if i + 1 < n && chars[i] == '_' && chars[i + 1] == '_' && !no_more_double_under {
            if let Some(close_offset) = find_char_seq(&chars[i + 2..], &['_', '_']) {
                if close_offset > 0 {
                    flush_plain(&mut plain_buf, &mut spans);
                    let inner: String = chars[i + 2..i + 2 + close_offset].iter().collect();
                    for mut sub in tokenize_inline_spans(&inner) {
                        sub.bold = true;
                        spans.push(sub);
                    }
                    i += 2 + close_offset + 2;
                    continue;
                }
            } else {
                no_more_double_under = true;
            }
        }

        // 6. Strikethrough `~~...~~`
        if i + 1 < n && chars[i] == '~' && chars[i + 1] == '~' && !no_more_double_tilde {
            if let Some(close_offset) = find_char_seq(&chars[i + 2..], &['~', '~']) {
                if close_offset > 0 {
                    flush_plain(&mut plain_buf, &mut spans);
                    let inner: String = chars[i + 2..i + 2 + close_offset].iter().collect();
                    for mut sub in tokenize_inline_spans(&inner) {
                        sub.strikethrough = true;
                        spans.push(sub);
                    }
                    i += 2 + close_offset + 2;
                    continue;
                }
            } else {
                no_more_double_tilde = true;
            }
        }

        // 7. Italic `*...*`
        if chars[i] == '*' && i + 1 < n && !chars[i + 1].is_whitespace() && !no_more_single_star {
            if let Some(close_offset) = find_char_seq(&chars[i + 1..], &['*']) {
                if close_offset > 0 && !chars[i + close_offset].is_whitespace() {
                    flush_plain(&mut plain_buf, &mut spans);
                    let inner: String = chars[i + 1..i + 1 + close_offset].iter().collect();
                    spans.push(InlineSpan {
                        text: inner,
                        bold: false,
                        italic: true,
                        code: false,
                        strikethrough: false,
                        math: false,
                        raw_math: String::new(),
                        link_url: String::new(),
                    });
                    i += 1 + close_offset + 1;
                    continue;
                }
            } else {
                no_more_single_star = true;
            }
        }

        // 8. Word-boundary Italic `_..._` (Fix U4: respects `snake_case` identifiers)
        let prev_is_alnum = i > 0 && chars[i - 1].is_alphanumeric();
        if chars[i] == '_'
            && !prev_is_alnum
            && i + 1 < n
            && !chars[i + 1].is_whitespace()
            && chars[i + 1] != '_'
            && !no_more_single_under
        {
            if let Some(close_offset) = find_closing_word_underscore(&chars[i + 1..]) {
                if close_offset > 0 {
                    flush_plain(&mut plain_buf, &mut spans);
                    let inner: String = chars[i + 1..i + 1 + close_offset].iter().collect();
                    spans.push(InlineSpan {
                        text: inner,
                        bold: false,
                        italic: true,
                        code: false,
                        strikethrough: false,
                        math: false,
                        raw_math: String::new(),
                        link_url: String::new(),
                    });
                    i += 1 + close_offset + 1;
                    continue;
                }
            } else if find_char_seq(&chars[i + 1..], &['_']).is_none() {
                no_more_single_under = true;
            }
        }

        // 9. Image `![alt](url)` (Fix U4 & M5)
        if chars[i] == '!' && i + 1 < n && chars[i + 1] == '[' {
            if let Some(close_bracket) = find_char_seq(&chars[i + 2..], &[']']) {
                let paren_start = i + 2 + close_bracket + 1;
                if paren_start < n && chars[paren_start] == '(' {
                    if let Some(close_paren) = find_char_seq(&chars[paren_start + 1..], &[')']) {
                        flush_plain(&mut plain_buf, &mut spans);
                        let alt: String = chars[i + 2..i + 2 + close_bracket].iter().collect();
                        let url: String = chars
                            [paren_start + 1..paren_start + 1 + close_paren]
                            .iter()
                            .collect();
                        let display = if alt.trim().is_empty() {
                            format!("[🖼 {}]", url)
                        } else {
                            format!("[🖼 {}]", alt)
                        };
                        spans.push(InlineSpan {
                            text: display,
                            bold: true,
                            italic: false,
                            code: false,
                            strikethrough: false,
                            math: false,
                            raw_math: String::new(),
                            link_url: url,
                        });
                        i = paren_start + 1 + close_paren + 1;
                        continue;
                    }
                }
            }
        }

        // 10. Markdown link `[label](url)` (Fix M5)
        if chars[i] == '[' {
            if let Some(close_bracket) = find_char_seq(&chars[i + 1..], &[']']) {
                let paren_start = i + 1 + close_bracket + 1;
                if paren_start < n && chars[paren_start] == '(' {
                    if let Some(close_paren) = find_char_seq(&chars[paren_start + 1..], &[')']) {
                        flush_plain(&mut plain_buf, &mut spans);
                        let label: String = chars[i + 1..i + 1 + close_bracket].iter().collect();
                        let url: String = chars
                            [paren_start + 1..paren_start + 1 + close_paren]
                            .iter()
                            .collect();
                        spans.push(InlineSpan {
                            text: label,
                            bold: true,
                            italic: false,
                            code: false,
                            strikethrough: false,
                            math: false,
                            raw_math: String::new(),
                            link_url: url,
                        });
                        i = paren_start + 1 + close_paren + 1;
                        continue;
                    }
                }
            }
        }

        // 11. Autolink `<https://...>` / `<http://...>` / `<mailto:...>` (Fix U4 & M5)
        if chars[i] == '<' {
            if let Some(close_angle) = find_char_seq(&chars[i + 1..], &['>']) {
                if close_angle > 4 {
                    let inner: String = chars[i + 1..i + 1 + close_angle].iter().collect();
                    if (inner.starts_with("http://")
                        || inner.starts_with("https://")
                        || inner.starts_with("mailto:"))
                        && !inner.chars().any(|c| c.is_whitespace())
                    {
                        flush_plain(&mut plain_buf, &mut spans);
                        spans.push(InlineSpan {
                            text: inner.clone(),
                            bold: false,
                            italic: false,
                            code: false,
                            strikethrough: false,
                            math: false,
                            raw_math: String::new(),
                            link_url: inner,
                        });
                        i += 1 + close_angle + 1;
                        continue;
                    }
                }
            }
        }

        plain_buf.push(chars[i]);
        i += 1;
    }

    flush_plain(&mut plain_buf, &mut spans);
    if spans.is_empty() && !input.is_empty() {
        spans.push(InlineSpan::plain(input.to_string()));
    }
    spans
}

fn find_char_seq(haystack: &[char], needle: &[char]) -> Option<usize> {
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

fn find_exact_backtick_run(haystack: &[char], count: usize) -> Option<usize> {
    let mut i = 0usize;
    while i < haystack.len() {
        if haystack[i] == '`' {
            let mut run = 1usize;
            while i + run < haystack.len() && haystack[i + run] == '`' {
                run += 1;
            }
            if run == count {
                return Some(i);
            }
            i += run;
        } else {
            i += 1;
        }
    }
    None
}

fn find_closing_word_underscore(haystack: &[char]) -> Option<usize> {
    for idx in 0..haystack.len() {
        if haystack[idx] == '_' {
            let prev_ok = idx > 0 && !haystack[idx - 1].is_whitespace();
            let next_ok = idx + 1 >= haystack.len() || !haystack[idx + 1].is_alphanumeric();
            if prev_ok && next_ok {
                return Some(idx);
            }
        }
    }
    None
}

/// Lay out inline spans into wrapped visual lines with:
/// - Word-boundary wrapping for English/ASCII words (Fix M3: never splits `world` into `wor`/`ld` unless the word itself exceeds `max_cols`);
/// - CJK character-level wrapping;
/// - Blank line preservation inside multi-line paragraphs (Fix M7).
pub fn layout_inline_runs_into_lines(
    text: &str,
    max_cols_per_line: usize,
) -> Vec<Vec<InlineSpan>> {
    let max_cols = max_cols_per_line.max(8);
    let mut all_lines: Vec<Vec<InlineSpan>> = Vec::new();

    for paragraph_line in text.split('\n') {
        let paragraph_line = paragraph_line.trim_end_matches('\r');
        // Fix M7: Preserve blank lines inside blocks as a visual empty line
        if paragraph_line.trim().is_empty() {
            all_lines.push(vec![InlineSpan::plain(" ".to_string())]);
            continue;
        }

        let spans = tokenize_inline_spans(paragraph_line);
        let mut current_line: Vec<InlineSpan> = Vec::new();
        let mut current_cols = 0usize;

        for span in spans {
            let span_cols: usize = span.text.chars().map(char_visual_cols).sum();
            // Keep inline code pills or short math/links intact if they fit on a line
            if (span.code || span.math || !span.link_url.is_empty()) && span_cols <= max_cols {
                let extra = if span.code { 2 } else { 0 };
                if current_cols + span_cols + extra > max_cols && !current_line.is_empty() {
                    all_lines.push(std::mem::take(&mut current_line));
                    current_cols = 0;
                }
                current_cols += span_cols + extra;
                current_line.push(span);
                continue;
            }

            // Fix M3: Tokenize span text into word/CJK/whitespace chunks for word-boundary wrapping
            let chunks = split_into_wrap_units(&span.text);
            let mut seg = String::new();

            for unit in chunks {
                let unit_cols: usize = unit.chars().map(char_visual_cols).sum();

                if current_cols + unit_cols > max_cols {
                    // Flush accumulated `seg` onto `current_line`, then push `current_line`
                    if !seg.is_empty() {
                        current_line.push(InlineSpan {
                            text: std::mem::take(&mut seg),
                            ..span.clone()
                        });
                    }
                    if !current_line.is_empty() {
                        all_lines.push(std::mem::take(&mut current_line));
                        current_cols = 0;
                    }

                    // Skip leading whitespace at the beginning of a wrapped line
                    if unit.chars().all(|c| c == ' ' || c == '\t') {
                        continue;
                    }

                    // If a single word is longer than `max_cols`, break it by character
                    if unit_cols > max_cols {
                        for ch in unit.chars() {
                            let w = char_visual_cols(ch);
                            if current_cols + w > max_cols && !seg.is_empty() {
                                current_line.push(InlineSpan {
                                    text: std::mem::take(&mut seg),
                                    ..span.clone()
                                });
                                all_lines.push(std::mem::take(&mut current_line));
                                current_cols = 0;
                            }
                            seg.push(ch);
                            current_cols += w;
                        }
                        continue;
                    }
                }

                seg.push_str(&unit);
                current_cols += unit_cols;
            }

            if !seg.is_empty() {
                current_line.push(InlineSpan {
                    text: seg,
                    ..span
                });
            }
        }

        if !current_line.is_empty() {
            all_lines.push(current_line);
        }
    }

    if all_lines.is_empty() {
        all_lines.push(vec![InlineSpan::plain(text.to_string())]);
    }
    all_lines
}

/// Calculate the visual character width of a string (CJK = 2 cols, ASCII = 1 col).
#[inline]
pub fn visual_char_width(s: &str) -> usize {
    s.chars().map(char_visual_cols).sum()
}

/// Split text into atomic wrap units:
/// - Contiguous ASCII words (letters, digits, punctuation attached to words) stay together;
/// - Whitespace runs;
/// - Individual CJK characters (which can wrap at any character boundary).
fn split_into_wrap_units(s: &str) -> Vec<String> {
    let mut units = Vec::new();
    let mut current_word = String::new();

    for ch in s.chars() {
        if ch.is_whitespace() {
            if !current_word.is_empty() {
                units.push(std::mem::take(&mut current_word));
            }
            units.push(ch.to_string());
        } else if char_visual_cols(ch) > 1 {
            // CJK character: can wrap before or after
            if !current_word.is_empty() {
                units.push(std::mem::take(&mut current_word));
            }
            units.push(ch.to_string());
        } else {
            current_word.push(ch);
        }
    }
    if !current_word.is_empty() {
        units.push(current_word);
    }
    units
}

/// Mermaid diagram node/edge step for native flowchart rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagramStep {
    pub from_label: String,
    pub edge_label: String,
    pub to_label: String,
}

/// Parse a Mermaid (`graph`/`flowchart`/`sequenceDiagram`/`stateDiagram`) source into structured steps.
/// Fix U2 & L4:
/// 1. Arrows are ordered strictly from longest to shortest (`"-.->"`, `"-->>"`, `"==>"`, `"-->"`, `"->>"`, `"->"`),
///    so `A -->> B` is never truncated by `-->`!
/// 2. Colon message splitting (`A ->> B: msg`) is restricted to Sequence/State diagrams or when no node brackets follow.
/// 3. Styling/directive lines (`classDef`, `class `, `style `, `linkStyle`, `click `, `direction `) are ignored.
/// 4. `participant A as Alice`, `actor A as Alice`, `state "Desc" as S1`, `note ...`, and `subgraph ...` are properly handled.
pub fn parse_mermaid_diagram(code: &str) -> (String, Vec<DiagramStep>) {
    let mut kind_title = "Mermaid Diagram".to_string();
    let mut is_sequence_or_state = false;
    let mut steps = Vec::new();
    let mut node_labels = std::collections::HashMap::<String, String>::new();

    // Longest-first arrow table (Fix L4)
    const ARROWS: &[&str] = &["-.->", "-->>", "==>", "-->", "->>", "->", "---"];

    for raw_line in code.lines() {
        let line = raw_line.trim().trim_end_matches(';');
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        if line.starts_with("flowchart") || line.starts_with("graph") {
            kind_title = format!("Flowchart ({})", line);
            is_sequence_or_state = false;
            continue;
        }
        if line.starts_with("sequenceDiagram") {
            kind_title = "Sequence Diagram".to_string();
            is_sequence_or_state = true;
            continue;
        }
        if line.starts_with("stateDiagram") {
            kind_title = "State Diagram".to_string();
            is_sequence_or_state = true;
            continue;
        }

        // Fix U2: Ignore styling and layout directive lines
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("classdef ")
            || lower.starts_with("class ")
            || lower.starts_with("style ")
            || lower.starts_with("linkstyle ")
            || lower.starts_with("click ")
            || lower.starts_with("direction ")
            || lower == "end"
            || lower.starts_with("autonumber")
            || lower.starts_with("activate ")
            || lower.starts_with("deactivate ")
        {
            continue;
        }

        // Handle `subgraph Title`
        if let Some(sub_rest) = line.strip_prefix("subgraph") {
            let sub_title = sub_rest.trim().trim_matches('"');
            if !sub_title.is_empty() {
                steps.push(DiagramStep {
                    from_label: format!("▣ {}", sub_title),
                    edge_label: String::new(),
                    to_label: String::new(),
                });
            }
            continue;
        }

        // Handle `participant A as Alice` / `actor A as Alice`
        if let Some(part_rest) = line
            .strip_prefix("participant ")
            .or_else(|| line.strip_prefix("actor "))
        {
            if let Some(as_idx) = part_rest.find(" as ") {
                let id = part_rest[..as_idx].trim().to_string();
                let alias = part_rest[as_idx + 4..].trim().trim_matches('"').to_string();
                node_labels.insert(id, alias);
            } else {
                let id = part_rest.trim().to_string();
                node_labels.insert(id.clone(), id);
            }
            continue;
        }

        // Handle `state "Description" as S1`
        if let Some(state_rest) = line.strip_prefix("state ") {
            if let Some(as_idx) = state_rest.find(" as ") {
                let desc = state_rest[..as_idx].trim().trim_matches('"').to_string();
                let id = state_rest[as_idx + 4..].trim().to_string();
                node_labels.insert(id, desc);
                continue;
            }
        }

        // Handle `note over/left of/right of X: text`
        if lower.starts_with("note ") {
            if let Some(colon_pos) = line.find(':') {
                let target_part = line[5..colon_pos].trim();
                let note_text = line[colon_pos + 1..].trim();
                steps.push(DiagramStep {
                    from_label: format!("Note ({})", target_part),
                    edge_label: String::new(),
                    to_label: note_text.to_string(),
                });
                continue;
            }
        }

        let mut matched = false;
        for &arrow in ARROWS {
            if let Some(pos) = line.find(arrow) {
                let left = line[..pos].trim();
                let mut right = line[pos + arrow.len()..].trim();
                let mut edge_label = String::new();

                if let Some(after_pipe) = right.strip_prefix('|') {
                    if let Some(end_pipe) = after_pipe.find('|') {
                        edge_label = after_pipe[..end_pipe].trim().to_string();
                        right = after_pipe[end_pipe + 1..].trim();
                    }
                } else if is_sequence_or_state
                    || (!right.contains('[') && !right.contains('{') && !right.contains('('))
                {
                    // Fix L4: Only split on `:` when in sequence/state diagram or when `right` has no node brackets
                    if let Some(colon_pos) = right.find(':') {
                        let target = right[..colon_pos].trim();
                        edge_label = right[colon_pos + 1..].trim().to_string();
                        right = target;
                    }
                }

                let from_clean = extract_mermaid_node(left, &mut node_labels);
                let to_clean = extract_mermaid_node(right, &mut node_labels);
                steps.push(DiagramStep {
                    from_label: from_clean,
                    edge_label,
                    to_label: to_clean,
                });
                matched = true;
                break;
            }
        }

        if !matched {
            let label = extract_mermaid_node(line, &mut node_labels);
            if !label.is_empty() {
                steps.push(DiagramStep {
                    from_label: label,
                    edge_label: String::new(),
                    to_label: String::new(),
                });
            }
        }
    }

    (kind_title, steps)
}

fn extract_mermaid_node(
    raw: &str,
    map: &mut std::collections::HashMap<String, String>,
) -> String {
    let s = raw.trim().trim_end_matches(';');
    for (open, close) in [('[', ']'), ('(', ')'), ('{', '}')] {
        if let Some(open_idx) = s.find(open) {
            if let Some(close_idx) = s.rfind(close) {
                if close_idx > open_idx {
                    let id = s[..open_idx].trim().to_string();
                    let label = s[open_idx + 1..close_idx]
                        .trim()
                        .trim_matches('"')
                        .trim_matches(['[', ']', '(', ')'])
                        .to_string();
                    if !id.is_empty() && !label.is_empty() {
                        map.insert(id, label.clone());
                        return label;
                    }
                }
            }
        }
    }
    if let Some(known) = map.get(s) {
        return known.clone();
    }
    s.trim_matches('"').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_l1_non_ascii_list_item_never_panics() {
        // Cyrillic (2-byte UTF-8) and Emoji (4-byte UTF-8)
        let m_ru = parse_list_marker("- привет мир").expect("Cyrillic list item");
        assert_eq!(&"- привет мир"[m_ru.content_byte_offset..], "привет мир");

        let m_emoji = parse_list_marker("- 😀 hello").expect("Emoji list item");
        assert_eq!(&"- 😀 hello"[m_emoji.content_byte_offset..], "😀 hello");

        let m_short = parse_list_marker("- я").expect("Single 2-byte char");
        assert_eq!(&"- я"[m_short.content_byte_offset..], "я");
    }

    #[test]
    fn test_u4_italic_underscore_image_and_autolink() {
        let s = "Hello _italic_ vs snake_case_ident and ![Diagram](https://a.com/x.png) plus <https://example.com>";
        let spans = tokenize_inline_spans(s);
        assert!(spans.iter().any(|sp| sp.italic && sp.text == "italic"));
        assert!(spans.iter().any(|sp| !sp.italic && sp.text.contains("snake_case_ident")));
        assert!(spans
            .iter()
            .any(|sp| sp.link_url == "https://a.com/x.png" && sp.text.contains("Diagram")));
        assert!(spans
            .iter()
            .any(|sp| sp.link_url == "https://example.com" && sp.text == "https://example.com"));
    }

    #[test]
    fn test_l11_atx_heading_preserves_csharp() {
        assert_eq!(parse_atx_heading("## C#"), Some((2, "C#")));
        assert_eq!(parse_atx_heading("## C# ##"), Some((2, "C#")));
        assert_eq!(parse_atx_heading("### F# Programming"), Some((3, "F# Programming")));
    }

    #[test]
    fn test_u3_l12_table_alignments_and_no_cell_truncation() {
        let md = "| Left | Center | Right |\n| :--- | :---: | ---: |\n| 1 | 2 | 3 | ExtraCell |";
        let (headers, aligns, rows) = parse_gfm_table_with_aligns(md);
        assert_eq!(headers.len(), 4);
        assert_eq!(&aligns[..3], &[0, 1, 2]);
        assert_eq!(rows[0][3], "ExtraCell");
    }

    #[test]
    fn test_l8_plain_text_preserves_code_and_formats_table() {
        let code_md = "```python\ndef foo(**kwargs):\n    x = `$HOME`\n```";
        assert_eq!(
            markdown_to_plain_text(code_md),
            "def foo(**kwargs):\n    x = `$HOME`"
        );
    }

    #[test]
    fn test_m3_word_boundary_wrapping() {
        let text = "Hello world wrapping verification";
        // Width 14: "Hello world " is 12 cols, "wrapping" is 8 cols -> must wrap before "wrapping", not mid-word!
        let lines = layout_inline_runs_into_lines(text, 14);
        assert_eq!(lines[0][0].text.trim(), "Hello world");
        assert!(lines[1][0].text.starts_with("wrapping"));
    }

    #[test]
    fn test_u2_l4_mermaid_long_arrows_and_directives() {
        let code = "\
sequenceDiagram
    participant A as Client
    participant B as Server
    A -->> B: Response OK
    classDef highlight fill:#f9f
";
        let (title, steps) = parse_mermaid_diagram(code);
        assert_eq!(title, "Sequence Diagram");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].from_label, "Client");
        assert_eq!(steps[0].to_label, "Server");
        assert_eq!(steps[0].edge_label, "Response OK");
    }
}
