//! # latex_renderer
//! Pure-Rust LaTeX math formula rasterization via `latex-rust` and `tiny-skia`,
//! with global `OnceLock<MathFont>` singleton, bounded `Arc<[u8]>` zero-copy PNG cache (Fix F5),
//! and recursive brace-aware Unicode fallback parser for `\frac{a}{b}`, `\sqrt{x}`, `\text{...}`,
//! `\left/\right`, and multi-character superscripts/subscripts (Fix U1).

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use latex_rust::font::MathFont;
use latex_rust::render::png::{latex_to_png, PngBackground, PngOptions};
use latex_rust::Color;
use parking_lot::RwLock;
use slint::Image;

const MAX_FORMULA_CACHE_ENTRIES: usize = 256;

type FormulaCacheKey = (String, bool, bool);
type FormulaCacheMap = HashMap<FormulaCacheKey, Arc<[u8]>>;

static MATH_FONT: OnceLock<Option<MathFont>> = OnceLock::new();
static FORMULA_PNG_CACHE: OnceLock<RwLock<FormulaCacheMap>> = OnceLock::new();

fn get_math_font() -> Option<&'static MathFont> {
    MATH_FONT
        .get_or_init(|| MathFont::stix_two_math().ok())
        .as_ref()
}

fn get_cache() -> &'static RwLock<FormulaCacheMap> {
    FORMULA_PNG_CACHE.get_or_init(|| RwLock::new(HashMap::with_capacity(64)))
}

/// Strip surrounding math delimiters (`$$`, `\[...\]`, `\(...\)`, `$`) from a formula string.
pub fn strip_math_delimiters(latex_src: &str) -> &str {
    let mut s = latex_src.trim();
    if let Some(inner) = s.strip_prefix("$$").and_then(|v| v.strip_suffix("$$")) {
        s = inner.trim();
    } else if let Some(inner) = s.strip_prefix("\\[").and_then(|v| v.strip_suffix("\\]")) {
        s = inner.trim();
    } else if let Some(inner) = s.strip_prefix("\\(").and_then(|v| v.strip_suffix("\\)")) {
        s = inner.trim();
    } else if let Some(inner) = s.strip_prefix('$').and_then(|v| v.strip_suffix('$')) {
        s = inner.trim();
    }
    s
}

/// Render a LaTeX math string into shared `Arc<[u8]>` PNG bytes (Fix F5: zero-copy cache hit & bounded capacity).
pub fn render_latex_to_png_bytes(
    latex_src: &str,
    dark_mode: bool,
    display: bool,
) -> Option<Arc<[u8]>> {
    let clean_latex = strip_math_delimiters(latex_src);
    if clean_latex.is_empty() {
        return None;
    }

    let key = (clean_latex.to_string(), dark_mode, display);
    if let Some(cached) = get_cache().read().get(&key) {
        return Some(cached.clone());
    }

    let font = get_math_font()?;

    let options = PngOptions {
        background: PngBackground::Transparent,
        color: if dark_mode {
            Color::rgb(232, 228, 217)
        } else {
            Color::rgb(43, 41, 38)
        },
        display,
        ..Default::default()
    };

    match latex_to_png(clean_latex, font, &options) {
        Ok(png_bytes) => {
            let shared: Arc<[u8]> = Arc::from(png_bytes);
            let mut cache = get_cache().write();
            if cache.len() >= MAX_FORMULA_CACHE_ENTRIES {
                cache.clear();
            }
            cache.insert(key, shared.clone());
            Some(shared)
        }
        Err(_) => None,
    }
}

/// Render a display LaTeX math string into a Slint `Image` (Must be called on the UI thread).
pub fn render_latex_to_image(latex_src: &str, dark_mode: bool) -> Option<Image> {
    let png_bytes = render_latex_to_png_bytes(latex_src, dark_mode, true)?;
    Image::load_from_data(&png_bytes, Some("png")).ok()
}

/// Render an inline LaTeX math string into a Slint `Image` (Must be called on the UI thread, Fix U6).
pub fn render_inline_latex_to_image(latex_src: &str, dark_mode: bool) -> Option<Image> {
    let png_bytes = render_latex_to_png_bytes(latex_src, dark_mode, false)?;
    Image::load_from_data(&png_bytes, Some("png")).ok()
}

/// Check if an inline formula contains structural constructs (`\frac`, `\sqrt`, `\sum`, `\int`, `\prod`, `\binom`)
/// that benefit from inline PNG hydration (`render_inline_latex_to_image`, Fix U6).
pub fn is_complex_inline_math(latex_src: &str) -> bool {
    latex_src.contains("\\frac")
        || latex_src.contains("\\sqrt")
        || latex_src.contains("\\sum")
        || latex_src.contains("\\int")
        || latex_src.contains("\\prod")
        || latex_src.contains("\\lim")
        || latex_src.contains("\\ матриц")
        || latex_src.contains("\\begin")
}

/// Fix U1: Recursive brace-aware LaTeX-to-Unicode converter supporting:
/// - `\frac{a}{b}` -> `(a)/(b)` (or `a/b` when atomic)
/// - `\sqrt{x}` -> `√(x)` (or `√x` when atomic)
/// - `\text{...}`, `\mathrm{...}`, `\mathbf{...}` -> inner text
/// - `\left` / `\right` removal
/// - `^{...}` and `_{...}` multi-character Unicode superscripts/subscripts
/// - Greek letters, operators, arrows, and symbols.
pub fn inline_latex_to_unicode(latex_src: &str) -> String {
    let clean = strip_math_delimiters(latex_src);
    let mut s = expand_latex_constructs(clean);

    // Strip sizing modifiers
    for modifier in [
        "\\left",
        "\\right",
        "\\bigl",
        "\\bigr",
        "\\Bigl",
        "\\Bigr",
        "\\displaystyle",
        "\\textstyle",
        "\\,",
        "\\;",
        "\\!",
    ] {
        s = s.replace(modifier, "");
    }

    let replacements: &[(&str, &str)] = &[
        ("\\alpha", "α"),
        ("\\beta", "β"),
        ("\\gamma", "γ"),
        ("\\delta", "δ"),
        ("\\varepsilon", "ε"),
        ("\\epsilon", "ε"),
        ("\\zeta", "ζ"),
        ("\\eta", "η"),
        ("\\theta", "θ"),
        ("\\iota", "ι"),
        ("\\kappa", "κ"),
        ("\\lambda", "λ"),
        ("\\mu", "μ"),
        ("\\nu", "ν"),
        ("\\xi", "ξ"),
        ("\\pi", "π"),
        ("\\rho", "ρ"),
        ("\\sigma", "σ"),
        ("\\tau", "τ"),
        ("\\varphi", "φ"),
        ("\\phi", "φ"),
        ("\\chi", "χ"),
        ("\\psi", "ψ"),
        ("\\omega", "ω"),
        ("\\Delta", "Δ"),
        ("\\Gamma", "Γ"),
        ("\\Lambda", "Λ"),
        ("\\Omega", "Ω"),
        ("\\Phi", "Φ"),
        ("\\Pi", "Π"),
        ("\\Psi", "Ψ"),
        ("\\Sigma", "Σ"),
        ("\\Theta", "Θ"),
        ("\\times", "×"),
        ("\\cdot", "·"),
        ("\\div", "÷"),
        ("\\pm", "±"),
        ("\\mp", "∓"),
        ("\\leq", "≤"),
        ("\\le", "≤"),
        ("\\geq", "≥"),
        ("\\ge", "≥"),
        ("\\neq", "≠"),
        ("\\ne", "≠"),
        ("\\approx", "≈"),
        ("\\equiv", "≡"),
        ("\\sim", "∼"),
        ("\\infty", "∞"),
        ("\\partial", "∂"),
        ("\\nabla", "∇"),
        ("\\forall", "∀"),
        ("\\exists", "∃"),
        ("\\notin", "∉"),
        ("\\in", "∈"),
        ("\\subseteq", "⊆"),
        ("\\subset", "⊂"),
        ("\\cup", "∪"),
        ("\\cap", "∩"),
        ("\\emptyset", "∅"),
        ("\\Leftrightarrow", "⇔"),
        ("\\leftrightarrow", "↔"),
        ("\\Rightarrow", "⇒"),
        ("\\Leftarrow", "⇐"),
        ("\\rightarrow", "→"),
        ("\\to", "→"),
        ("\\leftarrow", "←"),
        ("\\sum", "∑"),
        ("\\prod", "∏"),
        ("\\int", "∫"),
        ("\\sqrt", "√"),
    ];
    for (pat, rep) in replacements {
        s = s.replace(pat, rep);
    }

    convert_scripts_and_braces(&s)
}

fn expand_latex_constructs(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;

    while i < n {
        if chars[i] == '\\' {
            let mut cmd_end = i + 1;
            while cmd_end < n && chars[cmd_end].is_ascii_alphabetic() {
                cmd_end += 1;
            }
            let cmd: String = chars[i..cmd_end].iter().collect();

            if cmd == "\\frac" || cmd == "\\dfrac" || cmd == "\\tfrac" {
                if let Some((num, next_idx)) = extract_brace_group(&chars, cmd_end) {
                    if let Some((den, final_idx)) = extract_brace_group(&chars, next_idx) {
                        let num_u = inline_latex_to_unicode(&num);
                        let den_u = inline_latex_to_unicode(&den);
                        let num_fmt = if num_u.chars().count() > 1 && num_u.contains(['+', '-', ' ']) {
                            format!("({})", num_u)
                        } else {
                            num_u
                        };
                        let den_fmt = if den_u.chars().count() > 1 && den_u.contains(['+', '-', ' ', '·', '*']) {
                            format!("({})", den_u)
                        } else {
                            den_u
                        };
                        out.push_str(&format!("{}/{}", num_fmt, den_fmt));
                        i = final_idx;
                        continue;
                    }
                }
            } else if cmd == "\\sqrt" {
                // Optional `[n]` degree
                let mut cursor = cmd_end;
                while cursor < n && chars[cursor].is_whitespace() {
                    cursor += 1;
                }
                if cursor < n && chars[cursor] == '[' {
                    if let Some(close_br) = chars[cursor..].iter().position(|&c| c == ']') {
                        cursor += close_br + 1;
                    }
                }
                if let Some((rad, final_idx)) = extract_brace_group(&chars, cursor) {
                    let rad_u = inline_latex_to_unicode(&rad);
                    if rad_u.chars().count() <= 1 {
                        out.push_str(&format!("√{}", rad_u));
                    } else {
                        out.push_str(&format!("√({})", rad_u));
                    }
                    i = final_idx;
                    continue;
                }
            } else if matches!(
                cmd.as_str(),
                "\\text"
                    | "\\textrm"
                    | "\\textbf"
                    | "\\textit"
                    | "\\mathrm"
                    | "\\mathbf"
                    | "\\mathit"
                    | "\\mathcal"
                    | "\\mathbb"
            ) {
                if let Some((inner, final_idx)) = extract_brace_group(&chars, cmd_end) {
                    out.push_str(&inline_latex_to_unicode(&inner));
                    i = final_idx;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn extract_brace_group(chars: &[char], mut start: usize) -> Option<(String, usize)> {
    while start < chars.len() && chars[start].is_whitespace() {
        start += 1;
    }
    if start >= chars.len() || chars[start] != '{' {
        return None;
    }
    let mut depth = 0usize;
    let mut i = start;
    while i < chars.len() {
        if chars[i] == '{' {
            depth += 1;
        } else if chars[i] == '}' {
            depth -= 1;
            if depth == 0 {
                let inner: String = chars[start + 1..i].iter().collect();
                return Some((inner, i + 1));
            }
        }
        i += 1;
    }
    None
}

fn convert_scripts_and_braces(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;

    while i < n {
        if (chars[i] == '^' || chars[i] == '_') && i + 1 < n {
            let is_super = chars[i] == '^';
            if chars[i + 1] == '{' {
                if let Some((grp, next_idx)) = extract_brace_group(&chars, i + 1) {
                    let converted = map_script_str(&grp, is_super);
                    out.push_str(&converted);
                    i = next_idx;
                    continue;
                }
            } else {
                let single = chars[i + 1].to_string();
                out.push_str(&map_script_str(&single, is_super));
                i += 2;
                continue;
            }
        }
        if chars[i] != '{' && chars[i] != '}' {
            out.push(chars[i]);
        }
        i += 1;
    }
    out
}

fn map_script_str(s: &str, is_super: bool) -> String {
    let mut mapped = String::new();
    let mut all_ok = true;
    for ch in s.chars() {
        let rep = if is_super {
            match ch {
                '0' => Some('⁰'),
                '1' => Some('¹'),
                '2' => Some('²'),
                '3' => Some('³'),
                '4' => Some('⁴'),
                '5' => Some('⁵'),
                '6' => Some('⁶'),
                '7' => Some('⁷'),
                '8' => Some('⁸'),
                '9' => Some('⁹'),
                '+' => Some('⁺'),
                '-' => Some('⁻'),
                '=' => Some('⁼'),
                '(' => Some('⁽'),
                ')' => Some('⁾'),
                'n' => Some('ⁿ'),
                'i' => Some('ⁱ'),
                'T' => Some('ᵀ'),
                _ => None,
            }
        } else {
            match ch {
                '0' => Some('₀'),
                '1' => Some('₁'),
                '2' => Some('₂'),
                '3' => Some('₃'),
                '4' => Some('₄'),
                '5' => Some('₅'),
                '6' => Some('₆'),
                '7' => Some('₇'),
                '8' => Some('₈'),
                '9' => Some('₉'),
                '+' => Some('₊'),
                '-' => Some('₋'),
                '=' => Some('₌'),
                '(' => Some('₍'),
                ')' => Some('₎'),
                'a' => Some('ₐ'),
                'e' => Some('ₑ'),
                'i' => Some('ᵢ'),
                'j' => Some('ⱼ'),
                'k' => Some('ₖ'),
                'm' => Some('ₘ'),
                'n' => Some('ₙ'),
                'o' => Some('ₒ'),
                'p' => Some('ₚ'),
                'r' => Some('ᵣ'),
                's' => Some('ₛ'),
                't' => Some('ₜ'),
                'u' => Some('ᵤ'),
                'v' => Some('ᵥ'),
                'x' => Some('ₓ'),
                _ => None,
            }
        };
        if let Some(r) = rep {
            mapped.push(r);
        } else {
            all_ok = false;
            break;
        }
    }
    if all_ok && !mapped.is_empty() {
        mapped
    } else if is_super {
        format!("^({})", s)
    } else {
        format!("_({})", s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_simple_formula() {
        let formula = r"E = mc^2";
        let bytes = render_latex_to_png_bytes(formula, false, true);
        assert!(
            bytes.is_some(),
            "Formula E = mc^2 should render successfully"
        );
    }

    #[test]
    fn test_render_fraction_formula() {
        let formula = r"\[\frac{a + b}{c \cdot d}\]";
        let bytes = render_latex_to_png_bytes(formula, true, true);
        assert!(bytes.is_some(), "Fraction should render successfully");
    }

    #[test]
    fn test_u1_unicode_fallback_frac_sqrt_text_and_scripts() {
        let f = r"\left(\frac{a + b}{c}\right) + \sqrt{x^2 + 1} + \text{const}_{12}";
        let u = inline_latex_to_unicode(f);
        assert_eq!(u, "((a + b)/c) + √(x² + 1) + const₁₂");
    }
}
