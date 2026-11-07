//! # stream_automaton
//! Prefix-monotonic block state machine for two-phase streaming markdown rendering.
//! Faithfully implements the algorithm and invariants from `stream-block-automaton.ts`
//! and `agent-block-copy.ts`, while fixing audit issues:
//! - L5: Single-line `$$...$$` with trailing text on the same line never swallows subsequent content;
//! - L6: Only whitelisted custom block tags (`<think>`, `<artifact>`) enter `CustomTag` suspension;
//!   `<https://...>` autolinks and normal HTML tags stay in Markdown paragraphs;
//! - U9: Blockquotes and lists preserve nested code fences and structured sub-items;
//! - X2: 64KB high-water mark circuit breaker preserves `Fence`/`MathBlock` open context across splits;
//! - X8: Embedded `$$...$$` splitting recursively handles arbitrarily many display math formulas per block;
//! - F1: Zero-copy `advance()` API avoids cloning `closed_blocks` and `active_tail` every 16ms frame.

use crate::markdown_parser::{
    is_blank, is_fence_closer, is_quote_line, is_table_separator, is_thematic_break,
    leading_indent, parse_atx_heading, parse_fence_opener, parse_gfm_table_with_aligns,
    parse_list_marker, parse_setext_underline, parse_single_line_triple_code, strip_quote_prefix,
    FenceOpener, ListMarker,
};

const MAX_SUSPENSION_BYTES: usize = 64 * 1024;

/// Whitelist of custom paired tags that suspend streaming block closure (Fix L6).
/// Any other `<...>` (such as `<https://example.com>` or `<div>` or `<T>`) stays inside normal Markdown.
const CUSTOM_BLOCK_TAGS: &[&str] = &["think", "artifact"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItemEntry {
    pub indent_level: u8,
    pub marker: String,
    /// `0` = normal list item, `1` = unchecked `[ ]`, `2` = checked `[x]`
    pub task_state: u8,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockKind {
    Paragraph,
    Heading(u8),
    ThematicBreak,
    Blockquote {
        lines: Vec<String>,
    },
    List {
        items: Vec<ListItemEntry>,
    },
    Table {
        headers: Vec<String>,
        aligns: Vec<u8>,
        rows: Vec<Vec<String>>,
    },
    CodeBlock {
        lang: String,
        code: String,
        closed: bool,
    },
    Diagram {
        lang: String,
        code: String,
    },
    LatexMath {
        raw: String,
        closed: bool,
    },
    CustomTag {
        tag_name: String,
        content: String,
    },
    ActionCapsule {
        summary: String,
        detail_json: String,
        is_expanded: bool,
    },
    ActiveTail {
        is_code: bool,
        lang: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamBlock {
    pub id: String,
    pub kind: BlockKind,
    /// Clean display text or primary content.
    pub text: String,
    /// Raw original Markdown source slice for "Copy as Markdown" / "Copy as Plain Text" / "Copy as TSV".
    pub raw_markdown: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomatonFeedResult {
    pub closed_blocks: Vec<StreamBlock>,
    pub active_tail: Option<StreamBlock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenBlockType {
    Paragraph,
    Fence,
    MathBlock,
    Table,
    List,
    Quote,
    CustomTag,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawEmitKind {
    Markdown,
    Table,
    Code,
    Math,
    CustomTag,
}

/// Fix L6: Strictly parse only whitelisted custom block tags (`<think>`, `<artifact>`).
fn parse_open_tag(line: &str) -> Option<String> {
    let trimmed = line.trim_start_matches([' ', '\t']);
    if !trimmed.starts_with('<') || trimmed.starts_with("</") || trimmed.starts_with("<!") {
        return None;
    }
    let close_bracket = trimmed.find('>')?;
    let tag_header = &trimmed[1..close_bracket];
    if tag_header.trim_end().ends_with('/') {
        return None;
    }
    let name: String = tag_header
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if name.is_empty() {
        return None;
    }
    // Verify character immediately following tag name in `tag_header` is end-of-tag or whitespace
    // (rejects `<https://...>` where `:` follows `https`!)
    let after_name = &tag_header[name.len()..];
    if !after_name.is_empty() && !after_name.starts_with([' ', '\t']) {
        return None;
    }
    let lower = name.to_ascii_lowercase();
    if CUSTOM_BLOCK_TAGS.contains(&lower.as_str()) {
        Some(lower)
    } else {
        None
    }
}

fn is_math_block_opener(line: &str) -> Option<&'static str> {
    let trimmed = line.trim_start_matches([' ', '\t']).trim_end_matches('\r');
    if trimmed.starts_with("$$") {
        Some("$$")
    } else if trimmed.starts_with("\\[") {
        Some("\\]")
    } else {
        None
    }
}

fn is_stream_block_start_line(line: &str) -> bool {
    parse_fence_opener(line).is_some()
        || is_math_block_opener(line).is_some()
        || parse_open_tag(line).is_some()
        || parse_atx_heading(line).is_some()
        || is_quote_line(line)
        || parse_list_marker(line).is_some()
        || is_thematic_break(line)
        || parse_single_line_triple_code(line).is_some()
}

#[derive(Debug)]
pub struct StreamBlockAutomaton {
    max_suspension_bytes: usize,
    closed_blocks: Vec<StreamBlock>,
    next_id: usize,
    /// Monotonically increasing generation counter incremented whenever `reset()` occurs (Fix X6).
    generation: u64,
    line_start: usize,
    block_start: Option<usize>,
    open_type: Option<OpenBlockType>,
    emit_kind: RawEmitKind,
    fence: Option<(FenceOpener, String)>,
    math_closer: Option<&'static str>,
    custom_tag_name: Option<String>,
    last_non_blank_end: usize,
    paragraph_line_count: usize,
    last_line_had_pipe: bool,
    first_list_marker: Option<ListMarker>,
    /// Tracks nested code fence inside a list or quote block (Fix U9).
    nested_fence: Option<FenceOpener>,
    suspended: bool,
    last_text_len: usize,
    last_text_prefix_hash: u64,
    cached_done: bool,
    cached_tail: Option<StreamBlock>,
}

impl Default for StreamBlockAutomaton {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamBlockAutomaton {
    pub fn new() -> Self {
        Self {
            max_suspension_bytes: MAX_SUSPENSION_BYTES,
            closed_blocks: Vec::new(),
            next_id: 1,
            generation: 1,
            line_start: 0,
            block_start: None,
            open_type: None,
            emit_kind: RawEmitKind::Markdown,
            fence: None,
            math_closer: None,
            custom_tag_name: None,
            last_non_blank_end: 0,
            paragraph_line_count: 0,
            last_line_had_pipe: false,
            first_list_marker: None,
            nested_fence: None,
            suspended: false,
            last_text_len: 0,
            last_text_prefix_hash: 0,
            cached_done: false,
            cached_tail: None,
        }
    }

    pub fn reset(&mut self) {
        self.closed_blocks.clear();
        self.next_id = 1;
        self.generation = self.generation.wrapping_add(1);
        self.line_start = 0;
        self.block_start = None;
        self.open_type = None;
        self.emit_kind = RawEmitKind::Markdown;
        self.fence = None;
        self.math_closer = None;
        self.custom_tag_name = None;
        self.last_non_blank_end = 0;
        self.paragraph_line_count = 0;
        self.last_line_had_pipe = false;
        self.first_list_marker = None;
        self.nested_fence = None;
        self.suspended = false;
        self.last_text_len = 0;
        self.last_text_prefix_hash = 0;
        self.cached_done = false;
        self.cached_tail = None;
    }

    #[inline]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    #[inline]
    fn quick_hash(bytes: &[u8]) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        let step = (bytes.len() / 32).max(1);
        let mut i = 0;
        while i < bytes.len() {
            h ^= bytes[i] as u64;
            h = h.wrapping_mul(0x100000001b3);
            i += step;
        }
        h
    }

    pub fn add_action_capsule(&mut self, summary: String, detail_json: String) {
        self.add_action_capsule_with_state(summary, detail_json, false);
    }

    pub fn add_action_capsule_with_state(
        &mut self,
        summary: String,
        detail_json: String,
        is_expanded: bool,
    ) {
        let raw_md = format!("> ⚡ {}\n\n```json\n{}\n```", summary, detail_json);
        let block = StreamBlock {
            id: format!("capsule-{}", self.next_id),
            kind: BlockKind::ActionCapsule {
                summary: summary.clone(),
                detail_json,
                is_expanded,
            },
            text: summary,
            raw_markdown: raw_md,
        };
        self.next_id += 1;
        self.closed_blocks.push(block);
    }

    pub fn extract_capsules(&self) -> Vec<fugue_core::StoredActionCapsule> {
        let mut out = Vec::new();
        for b in &self.closed_blocks {
            if let BlockKind::ActionCapsule {
                ref summary,
                ref detail_json,
                is_expanded,
            } = b.kind
            {
                out.push(fugue_core::StoredActionCapsule {
                    summary: summary.clone(),
                    detail: detail_json.clone(),
                    is_expanded,
                });
            }
        }
        out
    }

    pub fn toggle_capsule(&mut self, block_id: &str) -> bool {
        for b in &mut self.closed_blocks {
            if b.id == block_id {
                if let BlockKind::ActionCapsule {
                    ref mut is_expanded,
                    ..
                } = b.kind
                {
                    *is_expanded = !*is_expanded;
                    return true;
                }
            }
        }
        false
    }

    /// Read-only slice of already closed blocks without cloning (Fix F1).
    #[inline]
    pub fn closed_blocks(&self) -> &[StreamBlock] {
        &self.closed_blocks
    }

    /// Read-only reference to current active tail without cloning (Fix F1).
    #[inline]
    pub fn active_tail(&self) -> Option<&StreamBlock> {
        self.cached_tail.as_ref()
    }

    /// Zero-copy incremental state advancement (Fix F1).
    /// Updates `self.closed_blocks` and `self.cached_tail` in place without cloning vectors.
    pub fn advance(&mut self, accumulated_text: &str, is_stream_done: bool) {
        if self.cached_done && !is_stream_done {
            self.reset();
        }

        if self.last_text_len > 0 {
            if accumulated_text.len() < self.last_text_len {
                self.reset();
            } else {
                let prefix_hash =
                    Self::quick_hash(&accumulated_text.as_bytes()[..self.last_text_len]);
                if prefix_hash != self.last_text_prefix_hash {
                    self.reset();
                }
            }
        }

        if accumulated_text.len() == self.last_text_len
            && is_stream_done == self.cached_done
            && self.last_text_len > 0
        {
            return;
        }

        self.scan_complete_lines(accumulated_text);

        if is_stream_done {
            self.resolve_suspension_with_partial(accumulated_text);
            self.flush_remainder(accumulated_text);
            self.last_text_len = accumulated_text.len();
            self.last_text_prefix_hash = Self::quick_hash(accumulated_text.as_bytes());
            self.cached_done = true;
            self.cached_tail = None;
            return;
        }

        self.resolve_suspension_with_partial(accumulated_text);
        self.enforce_high_water_mark(accumulated_text);
        self.last_text_len = accumulated_text.len();
        self.last_text_prefix_hash = Self::quick_hash(accumulated_text.as_bytes());
        self.cached_done = false;

        let tail_start = self.block_start.unwrap_or(self.line_start);
        let raw_tail = &accumulated_text[tail_start..];
        self.cached_tail = if raw_tail.trim().is_empty() {
            None
        } else {
            Some(self.build_active_tail_block(raw_tail))
        };
    }

    /// Convenience wrapper for unit tests that want owned `AutomatonFeedResult`.
    pub fn feed(&mut self, accumulated_text: &str, is_stream_done: bool) -> AutomatonFeedResult {
        self.advance(accumulated_text, is_stream_done);
        AutomatonFeedResult {
            closed_blocks: self.closed_blocks.clone(),
            active_tail: self.cached_tail.clone(),
        }
    }

    fn build_active_tail_block(&self, raw_tail: &str) -> StreamBlock {
        match self.open_type {
            Some(OpenBlockType::Fence) => {
                let lang = self
                    .fence
                    .as_ref()
                    .map(|(_, l)| l.clone())
                    .unwrap_or_default();
                let first_line = raw_tail.lines().next().unwrap_or("");
                let code_body = if parse_fence_opener(first_line).is_some() {
                    if let Some(nl) = raw_tail.find('\n') {
                        &raw_tail[nl + 1..]
                    } else {
                        ""
                    }
                } else {
                    raw_tail
                };
                StreamBlock {
                    id: format!("tail-{}", self.next_id),
                    kind: BlockKind::CodeBlock {
                        lang,
                        code: code_body.to_string(),
                        closed: false,
                    },
                    text: code_body.to_string(),
                    raw_markdown: raw_tail.to_string(),
                }
            }
            Some(OpenBlockType::MathBlock) => StreamBlock {
                id: format!("tail-{}", self.next_id),
                kind: BlockKind::LatexMath {
                    raw: raw_tail.to_string(),
                    closed: false,
                },
                text: raw_tail.to_string(),
                raw_markdown: raw_tail.to_string(),
            },
            _ => StreamBlock {
                id: format!("tail-{}", self.next_id),
                kind: BlockKind::ActiveTail {
                    is_code: false,
                    lang: String::new(),
                },
                text: raw_tail.to_string(),
                raw_markdown: raw_tail.to_string(),
            },
        }
    }

    fn scan_complete_lines(&mut self, text: &str) {
        let mut pos = self.line_start;
        while pos < text.len() {
            let Some(rel_nl) = text[pos..].find('\n') else {
                break;
            };
            let newline_index = pos + rel_nl;
            let line_end = newline_index + 1;
            let line = text[pos..newline_index].trim_end_matches('\r');
            self.handle_line(text, line, pos, line_end);
            pos = line_end;
            self.line_start = pos;
            self.enforce_high_water_mark(text);
        }
    }

    fn handle_line(&mut self, text: &str, line: &str, line_start: usize, line_end: usize) {
        if self.open_type == Some(OpenBlockType::Fence) {
            if let Some((opener, _)) = self.fence {
                if is_fence_closer(line, opener) {
                    if let Some(bs) = self.block_start {
                        self.emit(text, bs, line_end, RawEmitKind::Code);
                    }
                    self.clear_open();
                    return;
                }
            }
            if !is_blank(line) {
                self.last_non_blank_end = line_end;
            }
            return;
        }

        if self.open_type == Some(OpenBlockType::MathBlock) {
            if !is_blank(line) {
                self.last_non_blank_end = line_end;
            }
            if let Some(closer) = self.math_closer {
                if let Some(close_pos) = line.find(closer) {
                    if let Some(bs) = self.block_start {
                        let math_end = line_start + close_pos + closer.len();
                        self.emit(text, bs, math_end, RawEmitKind::Math);
                        let trailing = line[close_pos + closer.len()..].trim();
                        self.clear_open();
                        if !trailing.is_empty() {
                            self.emit(
                                text,
                                line_start + close_pos + closer.len(),
                                line_end,
                                RawEmitKind::Markdown,
                            );
                        }
                        return;
                    }
                }
            }
            return;
        }

        if self.open_type == Some(OpenBlockType::CustomTag) {
            if !is_blank(line) {
                self.last_non_blank_end = line_end;
            }
            if let Some(ref tag) = self.custom_tag_name {
                let close_pat = format!("</{}>", tag);
                if line.to_ascii_lowercase().contains(&close_pat) {
                    if let Some(bs) = self.block_start {
                        self.emit(text, bs, line_end, RawEmitKind::CustomTag);
                    }
                    self.clear_open();
                }
            }
            return;
        }

        if is_blank(line) {
            // If inside a nested fence within a list or quote, do not suspend on blank line (Fix U9)
            if self.nested_fence.is_some() {
                return;
            }
            if self.open_type == Some(OpenBlockType::Table) {
                if let Some(bs) = self.block_start {
                    self.emit(text, bs, self.last_non_blank_end, RawEmitKind::Table);
                }
                self.clear_open();
            } else if self.open_type.is_some() {
                self.suspended = true;
            }
            return;
        }

        if self.suspended {
            self.handle_suspended_line(text, line, line_start, line_end);
            return;
        }

        let Some(open_type) = self.open_type else {
            self.start_block(text, line, line_start, line_end);
            return;
        };

        match open_type {
            OpenBlockType::Paragraph => {
                self.handle_paragraph_continuation(text, line, line_start, line_end);
            }
            OpenBlockType::Quote => {
                if is_quote_line(line) {
                    self.last_non_blank_end = line_end;
                } else {
                    self.restart_block(text, line, line_start, line_end);
                }
            }
            OpenBlockType::List => {
                self.handle_list_continuation(text, line, line_start, line_end);
            }
            OpenBlockType::Table => {
                if line.contains('|') {
                    self.last_non_blank_end = line_end;
                } else {
                    self.restart_block(text, line, line_start, line_end);
                }
            }
            OpenBlockType::Fence | OpenBlockType::MathBlock | OpenBlockType::CustomTag => {}
        }
    }

    fn start_block(&mut self, text: &str, line: &str, line_start: usize, line_end: usize) {
        if let Some((opener, lang)) = parse_fence_opener(line) {
            self.begin_block(
                OpenBlockType::Fence,
                RawEmitKind::Code,
                line_start,
                line_end,
            );
            self.fence = Some((opener, lang));
            return;
        }

        if parse_single_line_triple_code(line).is_some() {
            self.emit(text, line_start, line_end, RawEmitKind::Code);
            return;
        }

        // Fix L5: Display math block `$$...$$` or `\[...\]`
        // If the closing delimiter appears on the SAME line (even with trailing text after `$$`),
        // immediately emit the formula (and any trailing text) instead of entering an unclosed MathBlock!
        if let Some(closer) = is_math_block_opener(line) {
            let ws_len = leading_indent(line);
            let opener_len = 2;
            let after_opener_start = line_start + ws_len + opener_len;
            let rest = &line[ws_len + opener_len..];
            if let Some(close_rel) = rest.find(closer) {
                let math_end = after_opener_start + close_rel + closer.len();
                self.emit(text, line_start, math_end, RawEmitKind::Math);
                let trailing = rest[close_rel + closer.len()..].trim();
                if !trailing.is_empty() {
                    self.emit(text, math_end, line_end, RawEmitKind::Markdown);
                }
                return;
            }
            self.begin_block(
                OpenBlockType::MathBlock,
                RawEmitKind::Math,
                line_start,
                line_end,
            );
            self.math_closer = Some(closer);
            return;
        }

        if let Some(tag_name) = parse_open_tag(line) {
            self.begin_block(
                OpenBlockType::CustomTag,
                RawEmitKind::CustomTag,
                line_start,
                line_end,
            );
            let close_pat = format!("</{}>", tag_name);
            self.custom_tag_name = Some(tag_name);
            if line.to_ascii_lowercase().contains(&close_pat) {
                if let Some(bs) = self.block_start {
                    self.emit(text, bs, line_end, RawEmitKind::CustomTag);
                }
                self.clear_open();
            }
            return;
        }

        if parse_atx_heading(line).is_some() || is_thematic_break(line) {
            self.emit(text, line_start, line_end, RawEmitKind::Markdown);
            return;
        }

        if is_quote_line(line) {
            self.begin_block(
                OpenBlockType::Quote,
                RawEmitKind::Markdown,
                line_start,
                line_end,
            );
            return;
        }

        if let Some(marker) = parse_list_marker(line) {
            self.begin_block(
                OpenBlockType::List,
                RawEmitKind::Markdown,
                line_start,
                line_end,
            );
            self.first_list_marker = Some(marker);
            return;
        }

        self.begin_block(
            OpenBlockType::Paragraph,
            RawEmitKind::Markdown,
            line_start,
            line_end,
        );
        self.paragraph_line_count = 1;
        self.last_line_had_pipe = line.contains('|');
    }

    fn begin_block(
        &mut self,
        open_type: OpenBlockType,
        emit_kind: RawEmitKind,
        start: usize,
        end: usize,
    ) {
        self.open_type = Some(open_type);
        self.emit_kind = emit_kind;
        self.block_start = Some(start);
        self.last_non_blank_end = end;
        self.suspended = false;
        self.paragraph_line_count = 0;
        self.last_line_had_pipe = false;
        self.first_list_marker = None;
        self.nested_fence = None;
    }

    fn handle_paragraph_continuation(
        &mut self,
        text: &str,
        line: &str,
        line_start: usize,
        line_end: usize,
    ) {
        if self.paragraph_line_count == 1 && parse_setext_underline(line).is_some() {
            self.last_non_blank_end = line_end;
            if let Some(bs) = self.block_start {
                self.emit(text, bs, line_end, RawEmitKind::Markdown);
            }
            self.clear_open();
            return;
        }

        if self.paragraph_line_count == 1 && self.last_line_had_pipe && is_table_separator(line) {
            self.open_type = Some(OpenBlockType::Table);
            self.emit_kind = RawEmitKind::Table;
            self.last_non_blank_end = line_end;
            return;
        }

        if is_stream_block_start_line(line) {
            self.restart_block(text, line, line_start, line_end);
            return;
        }

        self.last_non_blank_end = line_end;
        self.paragraph_line_count += 1;
        self.last_line_had_pipe = line.contains('|');
    }

    fn handle_list_continuation(
        &mut self,
        text: &str,
        line: &str,
        line_start: usize,
        line_end: usize,
    ) {
        // Fix U9: Track nested code fences inside indented list items
        if let Some(nf) = self.nested_fence {
            if is_fence_closer(line, nf) {
                self.nested_fence = None;
            }
            self.last_non_blank_end = line_end;
            return;
        }

        let Some(ref first) = self.first_list_marker else {
            self.restart_block(text, line, line_start, line_end);
            return;
        };
        let first_content_indent = first.content_indent;
        let first_empty = first.empty;

        if let Some(marker) = parse_list_marker(line) {
            if marker.indent >= first_content_indent {
                self.last_non_blank_end = line_end;
            } else {
                self.restart_block(text, line, line_start, line_end);
            }
            return;
        }

        if leading_indent(line) >= first_content_indent {
            if let Some((opener, _)) = parse_fence_opener(line) {
                self.nested_fence = Some(opener);
            }
            self.last_non_blank_end = line_end;
            return;
        }

        if !first_empty && !is_stream_block_start_line(line) {
            self.last_non_blank_end = line_end;
            return;
        }

        self.restart_block(text, line, line_start, line_end);
    }

    fn handle_suspended_line(
        &mut self,
        text: &str,
        line: &str,
        line_start: usize,
        line_end: usize,
    ) {
        let open_type = self.open_type;
        self.suspended = false;

        if open_type == Some(OpenBlockType::Paragraph) {
            if parse_setext_underline(line).is_some() {
                self.last_non_blank_end = line_end;
                if let Some(bs) = self.block_start {
                    self.emit(text, bs, line_end, RawEmitKind::Markdown);
                }
                self.clear_open();
                return;
            }
            self.restart_block(text, line, line_start, line_end);
            return;
        }

        if open_type == Some(OpenBlockType::Quote) {
            if is_quote_line(line) {
                self.last_non_blank_end = line_end;
                return;
            }
            self.restart_block(text, line, line_start, line_end);
            return;
        }

        if open_type == Some(OpenBlockType::List) {
            if let Some(ref first) = self.first_list_marker {
                let first_content_indent = first.content_indent;
                if let Some(marker) = parse_list_marker(line) {
                    if marker.indent >= first_content_indent {
                        self.last_non_blank_end = line_end;
                        return;
                    }
                    self.restart_block(text, line, line_start, line_end);
                    return;
                }
                if leading_indent(line) >= first_content_indent {
                    if let Some((opener, _)) = parse_fence_opener(line) {
                        self.nested_fence = Some(opener);
                    }
                    self.last_non_blank_end = line_end;
                    return;
                }
            }
            self.restart_block(text, line, line_start, line_end);
            return;
        }

        self.restart_block(text, line, line_start, line_end);
    }

    fn restart_block(&mut self, text: &str, line: &str, line_start: usize, line_end: usize) {
        self.close_open(text);
        self.start_block(text, line, line_start, line_end);
    }

    fn close_open(&mut self, text: &str) {
        if let Some(bs) = self.block_start {
            self.emit(text, bs, self.last_non_blank_end, self.emit_kind);
        }
        self.clear_open();
    }

    fn clear_open(&mut self) {
        self.open_type = None;
        self.block_start = None;
        self.emit_kind = RawEmitKind::Markdown;
        self.fence = None;
        self.math_closer = None;
        self.custom_tag_name = None;
        self.last_non_blank_end = 0;
        self.paragraph_line_count = 0;
        self.last_line_had_pipe = false;
        self.first_list_marker = None;
        self.nested_fence = None;
        self.suspended = false;
    }

    fn resolve_suspension_with_partial(&mut self, text: &str) {
        if !self.suspended || self.open_type.is_none() {
            return;
        }
        let partial = &text[self.line_start..];
        let trimmed = partial.trim_start();
        if trimmed.is_empty() {
            return;
        }
        let first = trimmed.as_bytes()[0];
        if first == b'=' || first == b'-' {
            return;
        }

        if self.open_type == Some(OpenBlockType::List) {
            if let Some(ref first_marker) = self.first_list_marker {
                if leading_indent(partial) >= first_marker.content_indent {
                    return;
                }
            }
            self.close_open(text);
            return;
        }

        if self.open_type == Some(OpenBlockType::Quote) && is_quote_line(partial) {
            return;
        }

        self.close_open(text);
    }

    /// Fix X2: 64KB high-water mark circuit breaker preserves `Fence`/`MathBlock` open state
    /// so code blocks exceeding 64KB remain classified as `CodeBlock` in subsequent chunks!
    fn enforce_high_water_mark(&mut self, text: &str) {
        loop {
            let tail_start = self.block_start.unwrap_or(self.line_start);
            if text.len().saturating_sub(tail_start) <= self.max_suspension_bytes {
                return;
            }

            let limit = tail_start + self.max_suspension_bytes;
            let mut safe_limit = limit.min(text.len());
            while safe_limit > tail_start && !text.is_char_boundary(safe_limit) {
                safe_limit -= 1;
            }

            let slice = &text[tail_start..safe_limit];
            let (next_line_start, source_end) = if let Some(rel_nl) = slice.rfind('\n') {
                let nl_idx = tail_start + rel_nl;
                (nl_idx + 1, nl_idx)
            } else {
                (safe_limit, safe_limit)
            };

            let prev_open_type = self.open_type;
            let prev_emit_kind = self.emit_kind;
            let prev_fence = self.fence.clone();
            let prev_math_closer = self.math_closer;
            let prev_custom_tag = self.custom_tag_name.clone();

            self.emit(text, tail_start, source_end, self.emit_kind);

            if matches!(
                prev_open_type,
                Some(OpenBlockType::Fence | OpenBlockType::MathBlock | OpenBlockType::CustomTag)
            ) {
                // Keep the enclosing fence/math/tag context alive for the remainder!
                self.open_type = prev_open_type;
                self.emit_kind = prev_emit_kind;
                self.fence = prev_fence;
                self.math_closer = prev_math_closer;
                self.custom_tag_name = prev_custom_tag;
                self.block_start = Some(next_line_start);
                self.last_non_blank_end = next_line_start;
                self.line_start = next_line_start;
            } else {
                self.clear_open();
                self.line_start = next_line_start;
            }
        }
    }

    fn flush_remainder(&mut self, text: &str) {
        let tail_start = self.block_start.unwrap_or(self.line_start);
        if tail_start < text.len() {
            self.emit(text, tail_start, text.len(), self.emit_kind);
        }
        self.clear_open();
        self.line_start = text.len();
    }

    fn emit(&mut self, text: &str, start: usize, end: usize, raw_kind: RawEmitKind) {
        if end <= start || end > text.len() {
            return;
        }
        let raw_source = text[start..end].trim_end_matches(['\r', '\n']);
        if raw_source.trim().is_empty() {
            return;
        }

        if raw_kind == RawEmitKind::Markdown {
            self.emit_markdown_with_embedded_math(raw_source);
            return;
        }

        self.push_classified_block(raw_source, raw_kind);
    }

    /// Fix X8: Recursively split arbitrarily many embedded `$$...$$` display math formulas
    /// inside a multi-line Markdown segment.
    fn emit_markdown_with_embedded_math(&mut self, raw_source: &str) {
        let trimmed_start = raw_source.trim_start();
        if !trimmed_start.starts_with("$$") {
            if let Some(math_idx) = raw_source.find("$$") {
                let after = &raw_source[math_idx + 2..];
                if let Some(close_idx) = after.find("$$") {
                    let before_str = &raw_source[..math_idx];
                    let formula_str = &after[..close_idx];
                    let after_str = &after[close_idx + 2..];
                    if before_str.contains('\n') || after_str.contains('\n') {
                        if !before_str.trim().is_empty() {
                            self.push_classified_block(
                                before_str.trim_matches(['\r', '\n']),
                                RawEmitKind::Markdown,
                            );
                        }
                        self.push_classified_block(
                            &format!("$${}$$", formula_str),
                            RawEmitKind::Math,
                        );
                        if !after_str.trim().is_empty() {
                            self.emit_markdown_with_embedded_math(
                                after_str.trim_matches(['\r', '\n']),
                            );
                        }
                        return;
                    }
                }
            }
        } else if let Some(after) = trimmed_start.strip_prefix("$$") {
            if let Some(close_idx) = after.find("$$") {
                let formula_str = &after[..close_idx];
                let remainder = after[close_idx + 2..].trim_matches(['\r', '\n']);
                if !remainder.is_empty() {
                    self.push_classified_block(
                        &format!("$${}$$", formula_str),
                        RawEmitKind::Math,
                    );
                    self.emit_markdown_with_embedded_math(remainder);
                    return;
                }
            }
        }

        self.push_classified_block(raw_source, RawEmitKind::Markdown);
    }

    fn push_classified_block(&mut self, source: &str, raw_kind: RawEmitKind) {
        let id = format!("block-{}", self.next_id);
        self.next_id += 1;
        let raw_markdown = source.to_string();

        let (kind, clean_text) = match raw_kind {
            RawEmitKind::Code => {
                if let Some(single_code) = parse_single_line_triple_code(source) {
                    (
                        BlockKind::CodeBlock {
                            lang: String::new(),
                            code: single_code.clone(),
                            closed: true,
                        },
                        single_code,
                    )
                } else {
                    let mut lines = source.lines();
                    let first_line = lines.next().unwrap_or("");
                    let (has_opener_line, opener_opt, lang) =
                        if let Some((op, l)) = parse_fence_opener(first_line) {
                            (true, Some(op), l)
                        } else if let Some((op, ref l)) = self.fence {
                            // Continuation chunk after 64KB high-water mark split (Fix X2)
                            (false, Some(op), l.clone())
                        } else {
                            (false, None, String::new())
                        };

                    let mut body_lines: Vec<&str> = if has_opener_line {
                        lines.collect()
                    } else {
                        source.lines().collect()
                    };

                    if let Some(opener) = opener_opt {
                        if let Some(&last) = body_lines.last() {
                            if is_fence_closer(last, opener) {
                                body_lines.pop();
                            }
                        }
                    }
                    let code = body_lines.join("\n");
                    let lang_lower = lang.to_ascii_lowercase();
                    if lang_lower == "mermaid" || lang_lower == "chart" || lang_lower == "graph" {
                        (
                            BlockKind::Diagram {
                                lang,
                                code: code.clone(),
                            },
                            code,
                        )
                    } else {
                        (
                            BlockKind::CodeBlock {
                                lang,
                                code: code.clone(),
                                closed: true,
                            },
                            code,
                        )
                    }
                }
            }
            RawEmitKind::Math => {
                let clean = crate::latex_renderer::strip_math_delimiters(source).to_string();
                (
                    BlockKind::LatexMath {
                        raw: clean.clone(),
                        closed: true,
                    },
                    clean,
                )
            }
            RawEmitKind::Table => {
                let (headers, aligns, rows) = parse_gfm_table_with_aligns(source);
                (
                    BlockKind::Table {
                        headers,
                        aligns,
                        rows,
                    },
                    source.to_string(),
                )
            }
            RawEmitKind::CustomTag => {
                let tag_name = parse_open_tag(source).unwrap_or_else(|| "think".to_string());
                (
                    BlockKind::CustomTag {
                        tag_name,
                        content: source.to_string(),
                    },
                    source.to_string(),
                )
            }
            RawEmitKind::Markdown => classify_markdown_block(source),
        };

        self.closed_blocks.push(StreamBlock {
            id,
            kind,
            text: clean_text,
            raw_markdown,
        });
    }
}

fn classify_markdown_block(source: &str) -> (BlockKind, String) {
    let trimmed = source.trim();
    let lines: Vec<&str> = source.lines().map(|l| l.trim_end_matches('\r')).collect();
    if lines.is_empty() {
        return (BlockKind::Paragraph, String::new());
    }

    // 1. Single-line Thematic Break (`---`, `***`, `___`)
    if lines.len() == 1 && is_thematic_break(lines[0]) {
        return (BlockKind::ThematicBreak, String::new());
    }

    // 2. Single-line ATX Heading (`# H1` .. `###### H6`)
    if lines.len() == 1 {
        if let Some((level, text)) = parse_atx_heading(lines[0]) {
            return (BlockKind::Heading(level), text.to_string());
        }
    }

    // 3. Single-line display math `$$...$$` or `\[...\]`
    if (trimmed.starts_with("$$") && trimmed.ends_with("$$") && trimmed.len() > 4)
        || (trimmed.starts_with("\\[") && trimmed.ends_with("\\]") && trimmed.len() > 4)
    {
        let clean = crate::latex_renderer::strip_math_delimiters(trimmed).to_string();
        return (
            BlockKind::LatexMath {
                raw: clean.clone(),
                closed: true,
            },
            clean,
        );
    }

    // 4. Setext Heading (Paragraph + optional blank lines + `===` or `---`)
    if lines.len() >= 2 {
        if let Some(level) = parse_setext_underline(lines.last().unwrap()) {
            let title_lines: Vec<&str> = lines[..lines.len() - 1]
                .iter()
                .copied()
                .filter(|l| !is_blank(l))
                .collect();
            if !title_lines.is_empty() {
                let heading_text = title_lines.join(" ").trim().to_string();
                return (BlockKind::Heading(level), heading_text);
            }
        }
    }

    // 5. Blockquote (`> ...`) - Fix U9: preserves inner code fences & list markers cleanly
    if is_quote_line(lines[0]) {
        let mut quote_lines = Vec::new();
        let mut in_quote_fence: Option<FenceOpener> = None;
        for l in &lines {
            let inner = strip_quote_prefix(l).unwrap_or_else(|| l.trim());
            if let Some(opener) = in_quote_fence {
                if is_fence_closer(inner, opener) {
                    in_quote_fence = None;
                } else {
                    quote_lines.push(format!("`{}`", inner));
                }
                continue;
            }
            if let Some((opener, _)) = parse_fence_opener(inner) {
                in_quote_fence = Some(opener);
                continue;
            }
            quote_lines.push(inner.to_string());
        }
        let joined = quote_lines.join("\n");
        return (BlockKind::Blockquote { lines: quote_lines }, joined);
    }

    // 6. List (Unordered `-`, `*`, `+`, Ordered `1.`, Task List `- [ ]`, `- [x]`)
    // Fix U9: Supports nested code fences inside list items by formatting fenced lines as inline code runs
    if parse_list_marker(lines[0]).is_some() {
        let mut items: Vec<ListItemEntry> = Vec::new();
        let mut in_item_fence: Option<FenceOpener> = None;

        for l in &lines {
            if is_blank(l) && in_item_fence.is_none() {
                continue;
            }
            if let Some(opener) = in_item_fence {
                if is_fence_closer(l, opener) {
                    in_item_fence = None;
                } else if let Some(last) = items.last_mut() {
                    last.text.push('\n');
                    last.text.push_str(&format!("`{}`", l.trim()));
                }
                continue;
            }
            if let Some((opener, _)) = parse_fence_opener(l) {
                in_item_fence = Some(opener);
                continue;
            }
            if let Some(m) = parse_list_marker(l) {
                let level = (m.indent / 2).min(4) as u8;
                let task_state = match m.task_checked {
                    None => 0,
                    Some(false) => 1,
                    Some(true) => 2,
                };
                let item_text = l[m.content_byte_offset..].trim().to_string();
                items.push(ListItemEntry {
                    indent_level: level,
                    marker: m.marker_label,
                    task_state,
                    text: item_text,
                });
            } else if let Some(last) = items.last_mut() {
                if !last.text.is_empty() {
                    last.text.push(' ');
                }
                last.text.push_str(l.trim());
            }
        }
        let summary_text = items
            .iter()
            .map(|it| it.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        return (BlockKind::List { items }, summary_text);
    }

    // 7. Fallback Paragraph
    (BlockKind::Paragraph, trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_paragraph_splitting_with_partial_tail() {
        let mut automaton = StreamBlockAutomaton::new();
        let input = "Hello world.\n\nThis is a second paragraph being typed";
        let res = automaton.feed(input, false);
        assert_eq!(res.closed_blocks.len(), 1);
        assert_eq!(res.closed_blocks[0].text, "Hello world.");
        assert!(res.active_tail.is_some());
        assert_eq!(
            res.active_tail.unwrap().text,
            "This is a second paragraph being typed"
        );
    }

    #[test]
    fn test_unclosed_fence_with_multiple_blank_lines() {
        let mut automaton = StreamBlockAutomaton::new();
        let chunk = "Intro:\n\n```rust\nfn a() {}\n\n\nfn b() {}\n";
        let res1 = automaton.feed(chunk, false);
        assert_eq!(res1.closed_blocks.len(), 1);
        assert_eq!(res1.closed_blocks[0].text, "Intro:");
        let tail = res1.active_tail.expect("Fence must remain in active tail");
        match tail.kind {
            BlockKind::CodeBlock { lang, closed, code } => {
                assert_eq!(lang, "rust");
                assert!(!closed);
                assert!(code.contains("fn a()"));
                assert!(code.contains("fn b()"));
            }
            _ => panic!("Expected unclosed CodeBlock in tail"),
        }

        let final_chunk = "Intro:\n\n```rust\nfn a() {}\n\n\nfn b() {}\n```\n";
        let res2 = automaton.feed(final_chunk, false);
        assert_eq!(res2.closed_blocks.len(), 2);
        assert!(res2.active_tail.is_none());
    }

    #[test]
    fn test_setext_heading_suspension_and_merge() {
        let mut automaton = StreamBlockAutomaton::new();
        let step1 = "Section Title\n\n";
        let res1 = automaton.feed(step1, false);
        assert_eq!(res1.closed_blocks.len(), 0);
        assert!(res1.active_tail.is_some());

        let step2 = "Section Title\n\n---\n";
        let res2 = automaton.feed(step2, false);
        assert_eq!(res2.closed_blocks.len(), 1);
        assert_eq!(res2.closed_blocks[0].kind, BlockKind::Heading(2));
        assert_eq!(res2.closed_blocks[0].text, "Section Title");
    }

    /// Fix X2: Verify >64KB code block splits into a closed CodeBlock AND preserves Fence state
    /// so the second half also closes as a CodeBlock (not a Paragraph!).
    #[test]
    fn test_x2_64kb_high_water_mark_preserves_fence_context() {
        let mut automaton = StreamBlockAutomaton::new();
        let mut huge = String::from("```rust\n");
        let line = "let x = 1234567890; // padding line to exceed 64KB\n";
        while huge.len() < 70 * 1024 {
            huge.push_str(line);
        }
        huge.push_str("fn tail_fn() {}\n```\n");
        let res = automaton.feed(&huge, true);
        assert!(res.closed_blocks.len() >= 2);
        for b in &res.closed_blocks {
            assert!(
                matches!(b.kind, BlockKind::CodeBlock { closed: true, .. }),
                "All split chunks of a >64KB fence must remain CodeBlock, got {:?}",
                b.kind
            );
        }
    }

    /// Fix L5: Single-line `$$E=mc^2$$ trailing explanation` must not swallow subsequent paragraphs.
    #[test]
    fn test_l5_single_line_math_with_trailing_text() {
        let mut automaton = StreamBlockAutomaton::new();
        let text = "$$E=mc^2$$ where E is energy.\n\nNext normal paragraph.";
        let res = automaton.feed(text, true);
        assert_eq!(res.closed_blocks.len(), 3);
        assert!(matches!(
            res.closed_blocks[0].kind,
            BlockKind::LatexMath { closed: true, .. }
        ));
        assert_eq!(res.closed_blocks[1].text, "where E is energy.");
        assert_eq!(res.closed_blocks[2].text, "Next normal paragraph.");
    }

    /// Fix L6: `<https://example.com>` and `<div>` must not enter CustomTag suspension and swallow text.
    #[test]
    fn test_l6_autolink_and_html_do_not_swallow_content() {
        let mut automaton = StreamBlockAutomaton::new();
        let text = "<https://example.com>\n\n<div>hello</div>\n\nFinal paragraph.";
        let res = automaton.feed(text, true);
        assert_eq!(res.closed_blocks.len(), 3);
        assert_eq!(res.closed_blocks[0].kind, BlockKind::Paragraph);
        assert_eq!(res.closed_blocks[1].kind, BlockKind::Paragraph);
        assert_eq!(res.closed_blocks[2].text, "Final paragraph.");
    }

    /// Fix X8: Multiple `$$...$$` blocks inside a single multi-line chunk are all split out.
    #[test]
    fn test_x8_multiple_embedded_display_math_in_one_chunk() {
        let mut automaton = StreamBlockAutomaton::new();
        let text = "Intro line\n$$a^2 + b^2 = c^2$$\nMiddle line\n$$x = y$$\nEnd line";
        let res = automaton.feed(text, true);
        let math_count = res
            .closed_blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::LatexMath { .. }))
            .count();
        assert_eq!(math_count, 2);
    }

    #[test]
    fn test_cascading_teardown_on_stream_done() {
        let mut automaton = StreamBlockAutomaton::new();
        let text = "First paragraph\n\nSecond unclosed paragraph";
        let res_streaming = automaton.feed(text, false);
        assert_eq!(res_streaming.closed_blocks.len(), 1);
        assert!(res_streaming.active_tail.is_some());

        let res_done = automaton.feed(text, true);
        assert_eq!(res_done.closed_blocks.len(), 2);
        assert!(res_done.active_tail.is_none());
    }

    #[test]
    fn test_thematic_break_and_setext_combo() {
        let mut automaton = StreamBlockAutomaton::new();
        let text = "---\nContent\n---";
        let res = automaton.feed(text, true);
        assert_eq!(res.closed_blocks.len(), 2);
        assert_eq!(res.closed_blocks[0].kind, BlockKind::ThematicBreak);
        assert_eq!(res.closed_blocks[1].kind, BlockKind::Heading(2));
        assert_eq!(res.closed_blocks[1].text, "Content");
    }

    #[test]
    fn test_all_10_syntaxes_classification() {
        let mut automaton = StreamBlockAutomaton::new();
        let doc = "\
**BoldText** and `InlineCode` and ~~StrikeText~~ with $E=mc^2$

- Normal item
- [ ] Unchecked task
- [x] Checked task

---
SetextTitle
---

> Blockquote line 1
> Blockquote line 2

| Feature | Status |
| :--- | :---: |
| Table | OK |

```Content```

```mermaid
flowchart LR
    A[Input] --> B[Output]
```

$$\\frac{1}{2}$$
";
        let res = automaton.feed(doc, true);
        assert!(res.active_tail.is_none());
        assert_eq!(res.closed_blocks.len(), 11);
        assert_eq!(res.closed_blocks[0].kind, BlockKind::Paragraph);
        assert_eq!(res.closed_blocks[4].kind, BlockKind::ThematicBreak);
        assert_eq!(res.closed_blocks[5].kind, BlockKind::Heading(2));
    }
}
