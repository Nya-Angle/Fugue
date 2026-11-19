//! # selection_engine
//! Chrome-parity cross-block character-level selection engine, hit-testing,
//! word/paragraph expansion, and partial-slice clipboard serialization.

use crate::text_metrics::{hit_test_x, measure_span_offsets, TextMetricStyle};
use std::time::Instant;

/// Logical cursor endpoint in the flattened conversation block stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TextPos {
    pub flat_index: usize,
    pub line_index: usize,
    pub char_offset: usize,
}

impl TextPos {
    pub const fn new(flat_index: usize, line_index: usize, char_offset: usize) -> Self {
        Self {
            flat_index,
            line_index,
            char_offset,
        }
    }
}

/// Active selection defined by an anchor and a moving focus endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSelection {
    pub anchor: TextPos,
    pub focus: TextPos,
}

impl TextSelection {
    pub fn ordered(&self) -> (TextPos, TextPos) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }

    pub fn is_collapsed(&self) -> bool {
        self.anchor == self.focus
    }
}

/// A single measured span within a selectable line.
#[derive(Debug, Clone)]
pub struct SelectableSpan {
    pub text: String,
    pub style: TextMetricStyle,
    pub font_size: f32,
    pub x_offset: f32,
    pub width: f32,
    pub char_start: usize,
    pub char_offsets: Vec<f32>,
}

/// A single visual line within a block with exact Y bounds and span geometry.
#[derive(Debug, Clone)]
pub struct SelectableLine {
    pub y_top: f32,
    pub height: f32,
    pub spans: Vec<SelectableSpan>,
    pub plain_text: String,
    pub char_count: usize,
    pub is_table_row: bool,
    pub table_cell_ranges: Vec<(usize, usize)>,
}

/// Complete selectable geometry for one flattened block.
#[derive(Debug, Clone)]
pub struct BlockSelectableGeometry {
    pub lines: Vec<SelectableLine>,
    pub is_atomic: bool,
    pub atomic_rect: (f32, f32, f32, f32),
}

/// Highlight rectangle in block-local coordinates (px).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HighlightRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Tracks pointer gesture state for deadzone filtering, multi-click detection, and edge auto-scroll.
#[derive(Debug, Clone)]
pub struct DragGestureState {
    pub is_pressed: bool,
    pub is_dragging: bool,
    pub press_viewport_x: f32,
    pub press_viewport_y: f32,
    pub last_viewport_x: f32,
    pub last_viewport_y: f32,
    pub click_count: u8,
    pub last_click_at: Option<Instant>,
    pub last_click_pos: (f32, f32),
}

impl Default for DragGestureState {
    fn default() -> Self {
        Self {
            is_pressed: false,
            is_dragging: false,
            press_viewport_x: 0.0,
            press_viewport_y: 0.0,
            last_viewport_x: 0.0,
            last_viewport_y: 0.0,
            click_count: 0,
            last_click_at: None,
            last_click_pos: (0.0, 0.0),
        }
    }
}

impl DragGestureState {
    pub const DEADZONE_PX: f32 = 3.5;

    pub fn register_press(&mut self, vx: f32, vy: f32) -> u8 {
        let now = Instant::now();
        let is_multi = self
            .last_click_at
            .map(|t| {
                now.duration_since(t).as_millis() <= 450
                    && (vx - self.last_click_pos.0).abs() <= 6.0
                    && (vy - self.last_click_pos.1).abs() <= 6.0
            })
            .unwrap_or(false);

        self.click_count = if is_multi {
            (self.click_count % 3) + 1
        } else {
            1
        };
        self.last_click_at = Some(now);
        self.last_click_pos = (vx, vy);
        self.is_pressed = true;
        self.is_dragging = false;
        self.press_viewport_x = vx;
        self.press_viewport_y = vy;
        self.last_viewport_x = vx;
        self.last_viewport_y = vy;
        self.click_count
    }

    /// Update pointer position during move. Returns `true` if drag selection is active.
    pub fn update_move(&mut self, vx: f32, vy: f32) -> bool {
        self.last_viewport_x = vx;
        self.last_viewport_y = vy;
        if !self.is_pressed {
            return false;
        }
        if !self.is_dragging {
            let dx = vx - self.press_viewport_x;
            let dy = vy - self.press_viewport_y;
            if (dx * dx + dy * dy).sqrt() >= Self::DEADZONE_PX {
                self.is_dragging = true;
            }
        }
        self.is_dragging
    }

    pub fn release(&mut self) {
        self.is_pressed = false;
        self.is_dragging = false;
    }
}

/// Build a `SelectableSpan` with exact glyph offsets.
pub fn make_span(
    text: String,
    style: TextMetricStyle,
    font_size: f32,
    x_offset: f32,
    char_start: usize,
    extra_pad_w: f32,
) -> SelectableSpan {
    let char_offsets = measure_span_offsets(&text, style, font_size);
    let width = char_offsets.last().copied().unwrap_or(0.0) + extra_pad_w;
    SelectableSpan {
        text,
        style,
        font_size,
        x_offset,
        width,
        char_start,
        char_offsets,
    }
}

/// Hit-test block-local coordinates `(local_x, local_y)` against a block's geometry.
pub fn hit_test_block(
    flat_index: usize,
    geom: &BlockSelectableGeometry,
    local_x: f32,
    local_y: f32,
) -> TextPos {
    if geom.is_atomic || geom.lines.is_empty() {
        let (_, ay, _, ah) = geom.atomic_rect;
        let char_off = if local_y >= ay + ah * 0.5 { 1 } else { 0 };
        return TextPos::new(flat_index, 0, char_off);
    }

    let mut line_idx = geom.lines.len() - 1;
    for (i, line) in geom.lines.iter().enumerate() {
        if local_y < line.y_top + line.height {
            line_idx = i;
            break;
        }
    }

    let line = &geom.lines[line_idx];
    if line.spans.is_empty() {
        return TextPos::new(flat_index, line_idx, 0);
    }

    // Before first span
    if local_x <= line.spans[0].x_offset {
        return TextPos::new(flat_index, line_idx, 0);
    }

    for span in &line.spans {
        if local_x <= span.x_offset + span.width {
            let rel_x = (local_x - span.x_offset).max(0.0);
            let local_c = hit_test_x(&span.char_offsets, rel_x);
            return TextPos::new(flat_index, line_idx, span.char_start + local_c);
        }
    }

    TextPos::new(flat_index, line_idx, line.char_count)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    AsciiWord,
    Cjk,
    Whitespace,
    Punctuation,
}

#[inline]
fn classify_char(ch: char) -> CharClass {
    if ch.is_whitespace() {
        CharClass::Whitespace
    } else if ch.is_ascii_alphanumeric() || ch == '_' {
        CharClass::AsciiWord
    } else if ('\u{4E00}'..='\u{9FFF}').contains(&ch)
        || ('\u{3400}'..='\u{4DBF}').contains(&ch)
        || ('\u{3040}'..='\u{30FF}').contains(&ch)
        || ('\u{AC00}'..='\u{D7AF}').contains(&ch)
    {
        CharClass::Cjk
    } else {
        CharClass::Punctuation
    }
}

/// Expand a cursor position to the surrounding word (for double-click selection).
pub fn expand_word_at(geom: &BlockSelectableGeometry, pos: TextPos) -> (TextPos, TextPos) {
    if geom.is_atomic || geom.lines.is_empty() {
        return (
            TextPos::new(pos.flat_index, 0, 0),
            TextPos::new(pos.flat_index, 0, 1),
        );
    }
    let line_idx = pos.line_index.min(geom.lines.len() - 1);
    let line = &geom.lines[line_idx];
    let chars: Vec<char> = line.plain_text.chars().collect();
    if chars.is_empty() {
        return (
            TextPos::new(pos.flat_index, line_idx, 0),
            TextPos::new(pos.flat_index, line_idx, 0),
        );
    }

    let pivot = pos.char_offset.min(chars.len() - 1);
    let cls = classify_char(chars[pivot]);
    let mut start = pivot;
    while start > 0 && classify_char(chars[start - 1]) == cls {
        start -= 1;
    }
    let mut end = pivot + 1;
    while end < chars.len() && classify_char(chars[end]) == cls {
        end += 1;
    }

    (
        TextPos::new(pos.flat_index, line_idx, start),
        TextPos::new(pos.flat_index, line_idx, end),
    )
}

/// Expand a cursor position to the entire block (for triple-click selection).
pub fn expand_block_range(flat_index: usize, geom: &BlockSelectableGeometry) -> (TextPos, TextPos) {
    if geom.is_atomic || geom.lines.is_empty() {
        return (TextPos::new(flat_index, 0, 0), TextPos::new(flat_index, 0, 1));
    }
    let last_line = geom.lines.len() - 1;
    let last_chars = geom.lines[last_line].char_count;
    (
        TextPos::new(flat_index, 0, 0),
        TextPos::new(flat_index, last_line, last_chars),
    )
}

/// Compute highlight rectangles in block-local coordinates for one visible block.
pub fn compute_block_highlight_rects(
    flat_index: usize,
    geom: &BlockSelectableGeometry,
    selection: TextSelection,
) -> Vec<HighlightRect> {
    if selection.is_collapsed() {
        return Vec::new();
    }
    let (start, end) = selection.ordered();
    if flat_index < start.flat_index || flat_index > end.flat_index {
        return Vec::new();
    }

    if geom.is_atomic || geom.lines.is_empty() {
        let covers_atomic = (flat_index > start.flat_index && flat_index < end.flat_index)
            || (flat_index == start.flat_index && flat_index == end.flat_index && start.char_offset == 0 && end.char_offset > 0)
            || (flat_index == start.flat_index && flat_index < end.flat_index && start.char_offset == 0)
            || (flat_index > start.flat_index && flat_index == end.flat_index && end.char_offset > 0);
        if covers_atomic {
            let (ax, ay, aw, ah) = geom.atomic_rect;
            return vec![HighlightRect {
                x: ax,
                y: ay,
                width: aw.max(8.0),
                height: ah.max(8.0),
            }];
        }
        return Vec::new();
    }

    let first_line = if flat_index == start.flat_index {
        start.line_index.min(geom.lines.len() - 1)
    } else {
        0
    };
    let last_line = if flat_index == end.flat_index {
        end.line_index.min(geom.lines.len() - 1)
    } else {
        geom.lines.len() - 1
    };

    let mut rects = Vec::with_capacity(last_line.saturating_sub(first_line) + 1);
    for l_idx in first_line..=last_line {
        let line = &geom.lines[l_idx];
        let c_start = if flat_index == start.flat_index && l_idx == first_line {
            start.char_offset.min(line.char_count)
        } else {
            0
        };
        let c_end = if flat_index == end.flat_index && l_idx == last_line {
            end.char_offset.min(line.char_count)
        } else {
            line.char_count
        };

        if c_start >= c_end || line.spans.is_empty() {
            // Show minimal caret-width highlight when dragging across blank lines
            if flat_index > start.flat_index && flat_index < end.flat_index && line.char_count == 0 {
                let base_x = line.spans.first().map(|s| s.x_offset).unwrap_or(6.0);
                rects.push(HighlightRect {
                    x: base_x,
                    y: line.y_top,
                    width: 6.0,
                    height: line.height,
                });
            }
            continue;
        }

        let x0 = char_offset_to_x(line, c_start);
        let x1 = char_offset_to_x(line, c_end);
        let w = (x1 - x0).max(2.0);
        rects.push(HighlightRect {
            x: x0,
            y: line.y_top,
            width: w,
            height: line.height,
        });
    }
    rects
}

fn char_offset_to_x(line: &SelectableLine, char_offset: usize) -> f32 {
    if line.spans.is_empty() {
        return 0.0;
    }
    for span in &line.spans {
        let span_chars = span.char_offsets.len().saturating_sub(1);
        let span_end = span.char_start + span_chars;
        if char_offset <= span_end {
            let local = char_offset.saturating_sub(span.char_start).min(span_chars);
            return span.x_offset + span.char_offsets[local];
        }
    }
    let last = line.spans.last().unwrap();
    last.x_offset + last.width
}

/// Extract selected plain text from a single block geometry.
pub fn extract_block_selected_text(
    flat_index: usize,
    geom: &BlockSelectableGeometry,
    fallback_raw: &str,
    selection: TextSelection,
) -> String {
    if selection.is_collapsed() {
        return String::new();
    }
    let (start, end) = selection.ordered();
    if flat_index < start.flat_index || flat_index > end.flat_index {
        return String::new();
    }
    if geom.is_atomic || geom.lines.is_empty() {
        return fallback_raw.to_string();
    }

    let first_line = if flat_index == start.flat_index {
        start.line_index.min(geom.lines.len() - 1)
    } else {
        0
    };
    let last_line = if flat_index == end.flat_index {
        end.line_index.min(geom.lines.len() - 1)
    } else {
        geom.lines.len() - 1
    };

    let mut out_lines = Vec::with_capacity(last_line.saturating_sub(first_line) + 1);
    for l_idx in first_line..=last_line {
        let line = &geom.lines[l_idx];
        let c_start = if flat_index == start.flat_index && l_idx == first_line {
            start.char_offset.min(line.char_count)
        } else {
            0
        };
        let c_end = if flat_index == end.flat_index && l_idx == last_line {
            end.char_offset.min(line.char_count)
        } else {
            line.char_count
        };

        if c_start >= c_end {
            if line.char_count == 0 {
                out_lines.push(String::new());
            }
            continue;
        }

        if line.is_table_row && !line.table_cell_ranges.is_empty() {
            let mut cells = Vec::new();
            for &(cs, ce) in &line.table_cell_ranges {
                let s = c_start.max(cs);
                let e = c_end.min(ce);
                if s < e {
                    let slice: String = line
                        .plain_text
                        .chars()
                        .skip(s)
                        .take(e - s)
                        .collect();
                    cells.push(slice.trim().to_string());
                } else if c_start <= cs && c_end >= ce {
                    cells.push(String::new());
                }
            }
            if !cells.is_empty() {
                out_lines.push(cells.join("\t"));
                continue;
            }
        }

        let slice: String = line
            .plain_text
            .chars()
            .skip(c_start)
            .take(c_end - c_start)
            .collect();
        out_lines.push(slice);
    }

    out_lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_geom() -> BlockSelectableGeometry {
        let span1 = make_span("Hello ".into(), TextMetricStyle::Regular, 14.0, 6.0, 0, 0.0);
        let span2_x = span1.x_offset + span1.width;
        let span2 = make_span("world!".into(), TextMetricStyle::Bold, 14.0, span2_x, 6, 0.0);
        let line = SelectableLine {
            y_top: 8.0,
            height: 20.0,
            spans: vec![span1, span2],
            plain_text: "Hello world!".into(),
            char_count: 12,
            is_table_row: false,
            table_cell_ranges: Vec::new(),
        };
        BlockSelectableGeometry {
            lines: vec![line],
            is_atomic: false,
            atomic_rect: (0.0, 0.0, 0.0, 0.0),
        }
    }

    #[test]
    fn test_word_expansion_and_extract() {
        let geom = sample_geom();
        let (s, e) = expand_word_at(&geom, TextPos::new(0, 0, 7));
        assert_eq!(s.char_offset, 6);
        assert_eq!(e.char_offset, 11);
        let sel = TextSelection { anchor: s, focus: e };
        let extracted = extract_block_selected_text(0, &geom, "", sel);
        assert_eq!(extracted, "world");
    }

    #[test]
    fn test_highlight_rects_non_empty() {
        let geom = sample_geom();
        let sel = TextSelection {
            anchor: TextPos::new(0, 0, 2),
            focus: TextPos::new(0, 0, 9),
        };
        let rects = compute_block_highlight_rects(0, &geom, sel);
        assert_eq!(rects.len(), 1);
        assert!(rects[0].width > 10.0);
        assert_eq!(rects[0].y, 8.0);
        assert_eq!(rects[0].height, 20.0);
    }
}
