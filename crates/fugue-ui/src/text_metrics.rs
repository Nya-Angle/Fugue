//! # text_metrics
//! Exact glyph advance measurement & character X-offset prefix tables using `fontdb` + `ttf-parser`.
//! Provides Chrome-parity character hit-testing and width-accurate text wrapping with zero heavy dependencies.

use fontdb::{Database, Family, Query, Source, Style, Weight, ID};
use memmap2::Mmap;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use ttf_parser::Face;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextMetricStyle {
    Regular,
    Bold,
    Italic,
    BoldItalic,
    Mono,
    MathSerif,
}

impl TextMetricStyle {
    #[inline]
    fn slot(self) -> u8 {
        match self {
            Self::Regular => 0,
            Self::Bold => 1,
            Self::Italic => 2,
            Self::BoldItalic => 3,
            Self::Mono => 4,
            Self::MathSerif => 5,
        }
    }
}

enum FontBytes {
    Mapped(Mmap),
    Owned(Vec<u8>),
}

impl FontBytes {
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Mapped(m) => m.as_ref(),
            Self::Owned(v) => v.as_slice(),
        }
    }
}

struct LoadedFont {
    data: Arc<FontBytes>,
    face_index: u32,
}

impl LoadedFont {
    fn as_face(&self) -> Option<Face<'_>> {
        Face::parse(self.data.as_slice(), self.face_index).ok()
    }
}

struct FontMetricEngine {
    primary: [Option<LoadedFont>; 6],
    cjk_fallback: Option<LoadedFont>,
    advance_cache: RwLock<HashMap<(u8, u16, char), f32>>,
}

static FONT_ENGINE: OnceLock<FontMetricEngine> = OnceLock::new();

fn load_face_data(
    db: &Database,
    id: ID,
    mmap_cache: &mut HashMap<PathBuf, Arc<FontBytes>>,
) -> Option<LoadedFont> {
    let (source, face_index) = db.face_source(id)?;
    match source {
        Source::File(path) => {
            if let Some(existing) = mmap_cache.get(&path) {
                return Some(LoadedFont {
                    data: existing.clone(),
                    face_index,
                });
            }
            let file = File::open(&path).ok()?;
            // SAFETY: System font files are read-only while mapped for glyph metric parsing.
            let mmap = unsafe { Mmap::map(&file).ok()? };
            let arc = Arc::new(FontBytes::Mapped(mmap));
            mmap_cache.insert(path, arc.clone());
            Some(LoadedFont {
                data: arc,
                face_index,
            })
        }
        Source::Binary(bin) => Some(LoadedFont {
            data: Arc::new(FontBytes::Owned(bin.as_ref().as_ref().to_vec())),
            face_index,
        }),
        Source::SharedFile(path, _) => {
            if let Some(existing) = mmap_cache.get(&path) {
                return Some(LoadedFont {
                    data: existing.clone(),
                    face_index,
                });
            }
            let file = File::open(&path).ok()?;
            // SAFETY: System font files are read-only while mapped for glyph metric parsing.
            let mmap = unsafe { Mmap::map(&file).ok()? };
            let arc = Arc::new(FontBytes::Mapped(mmap));
            mmap_cache.insert(path, arc.clone());
            Some(LoadedFont {
                data: arc,
                face_index,
            })
        }
    }
}

fn query_first(
    db: &Database,
    families: &[Family<'_>],
    weight: Weight,
    style: Style,
    mmap_cache: &mut HashMap<PathBuf, Arc<FontBytes>>,
) -> Option<LoadedFont> {
    let id = db.query(&Query {
        families,
        weight,
        stretch: fontdb::Stretch::Normal,
        style,
    })?;
    load_face_data(db, id, mmap_cache)
}

fn init_engine() -> FontMetricEngine {
    let mut db = Database::new();
    db.load_system_fonts();
    let mut mmap_cache: HashMap<PathBuf, Arc<FontBytes>> = HashMap::new();

    let sans_families = [
        Family::Name("Segoe UI"),
        Family::Name("Microsoft YaHei UI"),
        Family::Name("Microsoft YaHei"),
        Family::Name("Inter"),
        Family::SansSerif,
    ];
    let mono_families = [
        Family::Name("JetBrains Mono"),
        Family::Name("Consolas"),
        Family::Name("Cascadia Mono"),
        Family::Name("Courier New"),
        Family::Monospace,
    ];
    let math_families = [
        Family::Name("Cambria Math"),
        Family::Name("Times New Roman"),
        Family::Serif,
    ];
    let cjk_families = [
        Family::Name("Microsoft YaHei UI"),
        Family::Name("Microsoft YaHei"),
        Family::Name("SimHei"),
        Family::Name("PingFang SC"),
        Family::Name("Noto Sans CJK SC"),
        Family::SansSerif,
    ];

    let regular = query_first(&db, &sans_families, Weight::NORMAL, Style::Normal, &mut mmap_cache);
    let bold = query_first(&db, &sans_families, Weight::BOLD, Style::Normal, &mut mmap_cache)
        .or_else(|| query_first(&db, &sans_families, Weight::NORMAL, Style::Normal, &mut mmap_cache));
    let italic = query_first(&db, &sans_families, Weight::NORMAL, Style::Italic, &mut mmap_cache)
        .or_else(|| query_first(&db, &sans_families, Weight::NORMAL, Style::Normal, &mut mmap_cache));
    let bold_italic = query_first(&db, &sans_families, Weight::BOLD, Style::Italic, &mut mmap_cache)
        .or_else(|| query_first(&db, &sans_families, Weight::BOLD, Style::Normal, &mut mmap_cache));
    let mono = query_first(&db, &mono_families, Weight::NORMAL, Style::Normal, &mut mmap_cache)
        .or_else(|| query_first(&db, &sans_families, Weight::NORMAL, Style::Normal, &mut mmap_cache));
    let math = query_first(&db, &math_families, Weight::NORMAL, Style::Normal, &mut mmap_cache)
        .or_else(|| query_first(&db, &sans_families, Weight::NORMAL, Style::Normal, &mut mmap_cache));

    let cjk_fallback = query_first(&db, &cjk_families, Weight::NORMAL, Style::Normal, &mut mmap_cache);

    FontMetricEngine {
        primary: [regular, bold, italic, bold_italic, mono, math],
        cjk_fallback,
        advance_cache: RwLock::new(HashMap::with_capacity(2048)),
    }
}

fn engine() -> &'static FontMetricEngine {
    FONT_ENGINE.get_or_init(init_engine)
}

#[inline]
fn fallback_char_advance(ch: char, style: TextMetricStyle, font_size_px: f32) -> f32 {
    if ch == '\t' {
        return font_size_px * 0.6 * 4.0;
    }
    if style == TextMetricStyle::Mono {
        return if ch.is_ascii() {
            font_size_px * 0.6
        } else {
            font_size_px * 1.0
        };
    }
    if ch.is_ascii() {
        match ch {
            'i' | 'l' | '!' | '|' | '.' | ',' | ':' | ';' | '\'' => font_size_px * 0.28,
            ' ' => font_size_px * 0.28,
            'm' | 'w' | 'M' | 'W' => font_size_px * 0.78,
            c if c.is_ascii_uppercase() => font_size_px * 0.65,
            _ => font_size_px * 0.52,
        }
    } else {
        font_size_px * 1.0
    }
}

fn measure_from_loaded(font: &LoadedFont, ch: char, font_size_px: f32) -> Option<f32> {
    let face = font.as_face()?;
    let gid = face.glyph_index(ch)?;
    let units_per_em = face.units_per_em() as f32;
    if units_per_em <= 0.0 {
        return None;
    }
    let h_adv = face.glyph_hor_advance(gid)? as f32;
    Some((h_adv / units_per_em) * font_size_px)
}

/// Measure the horizontal advance width (in logical px) of a single character.
pub fn measure_char_advance(ch: char, style: TextMetricStyle, font_size_px: f32) -> f32 {
    if ch == '\n' || ch == '\r' {
        return 0.0;
    }
    let size_q = (font_size_px.max(1.0) * 4.0).round() as u16;
    let key = (style.slot(), size_q, ch);

    let eng = engine();
    if let Some(&cached) = eng.advance_cache.read().get(&key) {
        return cached;
    }

    let adv = if ch == '\t' {
        let space_w = eng.primary[style.slot() as usize]
            .as_ref()
            .and_then(|f| measure_from_loaded(f, ' ', font_size_px))
            .unwrap_or(font_size_px * 0.55);
        space_w * 4.0
    } else {
        eng.primary[style.slot() as usize]
            .as_ref()
            .and_then(|f| measure_from_loaded(f, ch, font_size_px))
            .or_else(|| {
                eng.cjk_fallback
                    .as_ref()
                    .and_then(|f| measure_from_loaded(f, ch, font_size_px))
            })
            .unwrap_or_else(|| fallback_char_advance(ch, style, font_size_px))
    };

    eng.advance_cache.write().insert(key, adv);
    adv
}

/// Compute cumulative X offsets for each character boundary in `text`.
/// The returned `Vec<f32>` always has length `text.chars().count() + 1` with `offsets[0] == 0.0`.
pub fn measure_span_offsets(text: &str, style: TextMetricStyle, font_size_px: f32) -> Vec<f32> {
    let char_count = text.chars().count();
    let mut offsets = Vec::with_capacity(char_count + 1);
    let mut acc = 0.0f32;
    offsets.push(0.0);
    for ch in text.chars() {
        acc += measure_char_advance(ch, style, font_size_px);
        offsets.push(acc);
    }
    offsets
}

/// Hit-test a local X coordinate against cumulative character boundary offsets,
/// snapping to the nearest glyph edge (midpoint rounding, matching Chrome behavior).
pub fn hit_test_x(offsets: &[f32], local_x: f32) -> usize {
    if offsets.len() <= 1 || local_x <= 0.0 {
        return 0;
    }
    let last_idx = offsets.len() - 1;
    if local_x >= offsets[last_idx] {
        return last_idx;
    }
    for i in 0..last_idx {
        let mid = (offsets[i] + offsets[i + 1]) * 0.5;
        if local_x < mid {
            return i;
        }
    }
    last_idx
}

/// Wrap plain text into visual lines constrained by `max_width_px` using exact glyph widths.
/// Used for `UserMessageBubble` so its line geometry matches Rust hit-testing 1:1.
pub fn wrap_plain_text_lines(text: &str, max_width_px: f32, font_size_px: f32) -> Vec<String> {
    let limit = max_width_px.max(40.0);
    let mut out = Vec::new();
    for raw_line in text.split('\n') {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            out.push(String::new());
            continue;
        }

        let mut current = String::new();
        let mut current_w = 0.0f32;
        let mut last_break_char_byte: Option<usize> = None;

        for ch in line.chars() {
            let adv = measure_char_advance(ch, TextMetricStyle::Regular, font_size_px);
            if !current.is_empty() && current_w + adv > limit {
                if let Some(break_byte) = last_break_char_byte {
                    let tail = current[break_byte..].trim_start().to_string();
                    current.truncate(break_byte);
                    let trimmed = current.trim_end().to_string();
                    if !trimmed.is_empty() {
                        out.push(trimmed);
                    }
                    current = tail;
                    current_w = current
                        .chars()
                        .map(|c| measure_char_advance(c, TextMetricStyle::Regular, font_size_px))
                        .sum();
                } else {
                    out.push(std::mem::take(&mut current));
                    current_w = 0.0;
                }
                last_break_char_byte = None;
            }
            current.push(ch);
            current_w += adv;
            if ch == ' ' || ch == '\t' {
                last_break_char_byte = Some(current.len());
            }
        }

        if !current.is_empty() || out.is_empty() {
            out.push(current);
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_measure_span_offsets_monotonic() {
        let offsets = measure_span_offsets("Hello 世界!", TextMetricStyle::Regular, 14.0);
        assert_eq!(offsets.len(), "Hello 世界!".chars().count() + 1);
        assert_eq!(offsets[0], 0.0);
        for w in offsets.windows(2) {
            assert!(w[1] > w[0], "offsets must strictly increase: {:?}", offsets);
        }
    }

    #[test]
    fn test_hit_test_x_midpoint_snapping() {
        let offsets = vec![0.0, 10.0, 20.0, 30.0];
        assert_eq!(hit_test_x(&offsets, -5.0), 0);
        assert_eq!(hit_test_x(&offsets, 4.9), 0);
        assert_eq!(hit_test_x(&offsets, 5.1), 1);
        assert_eq!(hit_test_x(&offsets, 14.9), 1);
        assert_eq!(hit_test_x(&offsets, 15.1), 2);
        assert_eq!(hit_test_x(&offsets, 99.0), 3);
    }

    #[test]
    fn test_wrap_plain_text_lines_respects_width() {
        let wrapped = wrap_plain_text_lines(
            "This is a sample message that should wrap cleanly across multiple lines.",
            160.0,
            14.0,
        );
        assert!(wrapped.len() >= 2);
    }
}
