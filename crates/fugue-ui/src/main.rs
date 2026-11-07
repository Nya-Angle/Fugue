//! # fugue-ui
//! Ultra-lightweight Slint-based GUI for Fugue conforming to AXIOM[02] & AXIOM[03].
//! Features:
//! - Multi-session isolated sandbox store (`C:\Users\<Username>\.fugue`) with `memmap2` zero-copy reads;
//! - 512-bit Cache-Line N-Gram Bitmap + AVX2 SIMD (`memchr::memmem`) cross-session indexer;
//! - Global Lenis (`duration = 1.2`, `lerp = 0.1`) smooth scrolling physics engine;
//! - Reserved `ClientControlGateway` for 9 LLM-callable client operations;
//! - Two-phase prefix-monotonic `StreamBlockAutomaton` with `FrozenBlock` memoization;
//! - $O(\log N)$ `FenwickViewportIndex` (Binary Indexed Tree + Binary Lifting) viewport culling;
//! - Real-time Slint measured block height feedback (`on_report_block_height`);
//! - Lazy viewport compilation on theme switch & dynamic column reflow;
//! - 16ms frame-throttled streaming batcher;
//! - Zero-allocation singleton context menu & left-drag block selection.

pub mod client_control;
pub mod deepseek_client;
pub mod highlighter;
pub mod latex_renderer;
pub mod markdown_parser;
pub mod smooth_scroll;
pub mod stream_automaton;
pub mod viewport_culler;

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use client_control::ClientControlGateway;
use deepseek_client::{ChatMessagePayload, DeepSeekClient, DeepSeekConfig, StreamEvent};
use fugue_core::{
    ClientControlCommand, ClientControlResponse, LanguagePreference, MessageRole,
    StoredActionCapsule, ThemePreference, UuidV7,
};
use fugue_kernel::{LivingNodeMemory, SessionSandboxStore};
use fugue_omni::OmniNode;
use markdown_parser::{
    layout_inline_runs_into_lines, markdown_to_plain_text, parse_mermaid_diagram,
    table_markdown_to_plain_text, table_markdown_to_tsv, visual_char_width,
};
use parking_lot::RwLock;
use slint::{Color, ComponentHandle, ModelRc, SharedString, VecModel};
use smooth_scroll::LenisScrollManager;
use stream_automaton::{BlockKind, StreamBlock, StreamBlockAutomaton};
use viewport_culler::FenwickViewportIndex;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

slint::include_modules!();

const DEFAULT_INLINE_COLS: usize = 68;
const VIEWPORT_OVERSCAN_PX: f32 = 240.0;

#[derive(Clone)]
struct CachedFrozenBlock {
    render_data: RenderBlockData,
    estimated_height: f32,
    dark_mode: bool,
    max_cols: usize,
    is_expanded: bool,
}

/// Line-level incremental cache for long unclosed `ActiveTail` paragraphs (Fix F2).
#[derive(Default, Clone)]
struct ActiveTailLineCache {
    msg_index: usize,
    max_cols: usize,
    dark_mode: bool,
    stable_prefix: String,
    stable_lines: Vec<RichLineData>,
}

struct MessageRecord {
    message_id: UuidV7,
    is_user: bool,
    accumulated_content: String,
    is_streaming: bool,
    persisted: bool,
    automaton: Arc<RwLock<StreamBlockAutomaton>>,
}

#[derive(Clone)]
struct FlatBlockEntry {
    flat_index: usize,
    msg_index: usize,
    block_index: usize,
    block_id: String,
    is_user: bool,
    is_active_tail: bool,
    raw_markdown: String,
    /// Raw code/text for code blocks so plain-text copy preserves exact source (Fix L8).
    plain_code_or_text: Option<String>,
    is_table: bool,
    is_first_in_msg: bool,
    source_block: Option<StreamBlock>,
    render_data: Option<RenderBlockData>,
}

struct UiViewportState {
    flat_blocks: Vec<FlatBlockEntry>,
    /// Permanent memoization cache for closed blocks (`(msg_index, block_id) -> CachedFrozenBlock`).
    frozen_cache: HashMap<(usize, String), CachedFrozenBlock>,
    /// Real measured heights reported from Slint layout (`(msg_index, block_id) -> height_px`).
    measured_heights: HashMap<(usize, String), f32>,
    /// Generation tracker per message to detect automaton resets (Fix X6).
    msg_generations: HashMap<usize, u64>,
    /// Incremental sync tracking (Fix F3).
    synced_msg_count: usize,
    synced_last_msg_closed: usize,
    had_active_tail: bool,
    active_tail_cache: ActiveTailLineCache,
    fenwick: FenwickViewportIndex,
    scroll_y: f32,
    viewport_height: f32,
    max_cols: usize,
    dark_mode: bool,
    selected_indices: BTreeSet<usize>,
    selection_anchor: Option<usize>,
    context_target_flat: Option<usize>,
    context_target_msg: Option<usize>,
}

impl Default for UiViewportState {
    fn default() -> Self {
        Self {
            flat_blocks: Vec::new(),
            frozen_cache: HashMap::new(),
            measured_heights: HashMap::new(),
            msg_generations: HashMap::new(),
            synced_msg_count: 0,
            synced_last_msg_closed: 0,
            had_active_tail: false,
            active_tail_cache: ActiveTailLineCache::default(),
            fenwick: FenwickViewportIndex::new(),
            scroll_y: 0.0,
            viewport_height: 540.0,
            max_cols: DEFAULT_INLINE_COLS,
            dark_mode: false,
            selected_indices: BTreeSet::new(),
            selection_anchor: None,
            context_target_flat: None,
            context_target_msg: None,
        }
    }
}

thread_local! {
    static UI_VIEWPORT_STATE: RefCell<UiViewportState> = RefCell::new(UiViewportState::default());
    static LENIS_STATE: RefCell<LenisScrollManager> = const { RefCell::new(LenisScrollManager::new()) };
    static LENIS_TIMER: slint::Timer = slint::Timer::default();
}

/// Starts the 120Hz (8ms) Lenis frame timer on demand and automatically stops it on sub-pixel convergence
/// so idle CPU usage is strictly 0%.
fn ensure_lenis_timer_running(app_weak: slint::Weak<MainWindow>) {
    LENIS_TIMER.with(|timer| {
        if !timer.running() {
            timer.start(
                slint::TimerMode::Repeated,
                Duration::from_millis(8),
                move || {
                    let Some(handle) = app_weak.upgrade() else {
                        LENIS_TIMER.with(|t| t.stop());
                        return;
                    };
                    let (chat_y, sidebar_y, input_y, _code_x, still_active) =
                        LENIS_STATE.with(|cell| {
                            let mut mgr = cell.borrow_mut();
                            let (c, s, i, h) = mgr.step_all();
                            (c, s, i, h, mgr.any_active())
                        });
                    if let Some(y) = chat_y {
                        handle.invoke_set_chat_scroll_y(y);
                    }
                    if let Some(y) = sidebar_y {
                        handle.invoke_set_sidebar_scroll_y(y);
                    }
                    if let Some(y) = input_y {
                        handle.invoke_set_input_scroll_y(y);
                    }
                    if !still_active {
                        LENIS_TIMER.with(|t| t.stop());
                    }
                },
            );
        }
    });
}

fn format_updated_time_label(updated_at_ms: u64) -> String {
    use chrono::{Local, TimeZone};
    let Some(dt) = Local.timestamp_millis_opt(updated_at_ms as i64).single() else {
        return String::new();
    };
    let now = Local::now();
    if dt.date_naive() == now.date_naive() {
        dt.format("%H:%M").to_string()
    } else {
        dt.format("%m-%d").to_string()
    }
}

fn theme_pref_to_slint(pref: ThemePreference) -> ThemeMode {
    match pref {
        ThemePreference::Light => ThemeMode::Light,
        ThemePreference::Dark => ThemeMode::Dark,
        ThemePreference::System => ThemeMode::System,
    }
}

fn slint_to_theme_pref(mode: ThemeMode) -> ThemePreference {
    match mode {
        ThemeMode::Light => ThemePreference::Light,
        ThemeMode::Dark => ThemePreference::Dark,
        ThemeMode::System => ThemePreference::System,
    }
}

fn lang_pref_to_str(pref: LanguagePreference) -> &'static str {
    match pref {
        LanguagePreference::Zh => "zh",
        LanguagePreference::En => "en",
    }
}

fn str_to_lang_pref(s: &str) -> LanguagePreference {
    if s.eq_ignore_ascii_case("en") {
        LanguagePreference::En
    } else {
        LanguagePreference::Zh
    }
}

fn refresh_search_hits_ui(window: &MainWindow, store: &SessionSandboxStore, query: &str) {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        window
            .global::<AppState>()
            .set_search_hits(ModelRc::new(VecModel::default()));
        return;
    }

    let is_zh = window.global::<AppState>().get_lang() == "zh";
    let hits = store.search_content(None, trimmed, 60);
    let items: Vec<SearchHitItemData> = hits
        .into_iter()
        .map(|h| {
            let role_label = match h.role {
                MessageRole::User => {
                    if is_zh {
                        "用户"
                    } else {
                        "User"
                    }
                }
                MessageRole::Assistant => "Omni",
                MessageRole::System => {
                    if is_zh {
                        "系统"
                    } else {
                        "System"
                    }
                }
            };
            SearchHitItemData {
                session_id: SharedString::from(h.session_id.to_string()),
                session_title: SharedString::from(h.session_title),
                message_id: SharedString::from(h.message_id.to_string()),
                message_index: h.message_index as i32,
                snippet: SharedString::from(h.matched_snippet),
                role_label: SharedString::from(role_label),
            }
        })
        .collect();

    window
        .global::<AppState>()
        .set_search_hits(ModelRc::new(VecModel::from(items)));
}

fn refresh_session_list_ui(window: &MainWindow, store: &SessionSandboxStore) {
    let state = window.global::<AppState>();
    let show_archived = state.get_show_archived_sessions();
    let current_sid_str = state.get_current_session_id().to_string();

    let metas = store.list_sessions(show_archived);
    let items: Vec<SessionItemData> = metas
        .into_iter()
        .map(|m| {
            let id_str = m.id.to_string();
            let active = id_str == current_sid_str;
            if active {
                state.set_current_session_title(SharedString::from(&m.title));
            }
            SessionItemData {
                id: SharedString::from(id_str),
                title: SharedString::from(m.title),
                preview: SharedString::from(m.preview_snippet),
                pinned: m.pinned,
                archived: m.archived,
                active,
                message_count: m.message_count as i32,
                updated_time_label: SharedString::from(format_updated_time_label(m.updated_at_ms)),
            }
        })
        .collect();

    state.set_session_list(ModelRc::new(VecModel::from(items)));

    let search_q = state.get_session_search_query().to_string();
    if !search_q.trim().is_empty() {
        refresh_search_hits_ui(window, store, &search_q);
    }
}

fn persist_assistant_record_if_needed(
    store: &SessionSandboxStore,
    session_id: UuidV7,
    rec: &mut MessageRecord,
) -> bool {
    if rec.is_user || rec.persisted {
        return false;
    }
    let capsules: Vec<StoredActionCapsule> = rec.automaton.read().extract_capsules();

    if rec.accumulated_content.trim().is_empty() && capsules.is_empty() {
        return false;
    }

    if let Ok(stored) = store.append_message(
        session_id,
        MessageRole::Assistant,
        &rec.accumulated_content,
        capsules,
    ) {
        rec.message_id = stored.id;
        rec.persisted = true;
        return true;
    }
    false
}

fn finalize_active_generation(
    window: &MainWindow,
    store: &SessionSandboxStore,
    records: &Arc<RwLock<Vec<MessageRecord>>>,
    stop_signal: &Arc<AtomicBool>,
) {
    stop_signal.store(true, Ordering::SeqCst);
    let state = window.global::<AppState>();
    if state.get_is_generating() {
        state.set_is_generating(false);
    }
    let current_sid = UuidV7::parse_str(state.get_current_session_id().as_str()).ok();
    let mut recs = records.write();
    if let Some(last) = recs.last_mut() {
        if last.is_streaming {
            last.is_streaming = false;
        }
        if let Some(sid) = current_sid {
            let _ = persist_assistant_record_if_needed(store, sid, last);
        }
    }
}

fn load_session_into_ui(
    window: &MainWindow,
    store: &SessionSandboxStore,
    records: &Arc<RwLock<Vec<MessageRecord>>>,
    session_id: UuidV7,
) {
    let Ok(meta) = store.get_session_metadata(session_id) else {
        return;
    };
    let paths = store.get_sandbox_paths(session_id).ok();
    let stored_msgs = store.load_session_messages(session_id).unwrap_or_default();
    let _ = store.update_config(|c| c.active_session_id = Some(session_id));

    let mut new_recs = Vec::with_capacity(stored_msgs.len());
    for m in stored_msgs {
        let is_user = m.role == MessageRole::User;
        let mut auto = StreamBlockAutomaton::new();
        if !is_user {
            auto.advance(&m.content, true);
            for cap in &m.capsules {
                auto.add_action_capsule_with_state(
                    cap.summary.clone(),
                    cap.detail.clone(),
                    cap.is_expanded,
                );
            }
        }
        new_recs.push(MessageRecord {
            message_id: m.id,
            is_user,
            accumulated_content: m.content,
            is_streaming: false,
            persisted: true,
            automaton: Arc::new(RwLock::new(auto)),
        });
    }

    UI_VIEWPORT_STATE.with(|cell| {
        let mut vp = cell.borrow_mut();
        vp.flat_blocks.clear();
        vp.frozen_cache.clear();
        vp.measured_heights.clear();
        vp.msg_generations.clear();
        vp.synced_msg_count = 0;
        vp.synced_last_msg_closed = 0;
        vp.had_active_tail = false;
        vp.active_tail_cache = ActiveTailLineCache::default();
        vp.fenwick = FenwickViewportIndex::new();
        vp.scroll_y = 0.0;
        vp.selected_indices.clear();
        vp.selection_anchor = None;
        vp.context_target_flat = None;
        vp.context_target_msg = None;
    });

    LENIS_STATE.with(|cell| {
        cell.borrow_mut().chat.jump_to(0.0, 0.0);
    });

    let state = window.global::<AppState>();
    state.set_current_session_id(SharedString::from(session_id.to_string()));
    state.set_current_session_title(SharedString::from(&meta.title));
    if let Some(p) = paths {
        state.set_current_sandbox_path(SharedString::from(
            p.root_dir.to_string_lossy().to_string(),
        ));
    }
    state.set_has_messages(!new_recs.is_empty());
    state.set_auto_scroll_sticky(true);
    state.set_renaming_session_id(SharedString::default());

    {
        let mut recs_guard = records.write();
        *recs_guard = new_recs;
        let is_dark = window.global::<Theme>().get_dark();
        sync_and_cull_ui(window, &recs_guard, is_dark);
    }

    window.invoke_set_chat_scroll_y(0.0);
    window.invoke_scroll_to_bottom_if_sticky();
    refresh_session_list_ui(window, store);
}

fn convert_spans_to_rich_lines(
    wrapped: Vec<Vec<markdown_parser::InlineSpan>>,
    dark_mode: bool,
) -> Vec<RichLineData> {
    wrapped
        .into_iter()
        .map(|spans| {
            let slint_spans: Vec<InlineSpanData> = spans
                .into_iter()
                .map(|sp| {
                    // Fix U6: Hydrate complex inline math via PNG renderer
                    let math_image =
                        if sp.math && latex_renderer::is_complex_inline_math(&sp.raw_math) {
                            latex_renderer::render_inline_latex_to_image(&sp.raw_math, dark_mode)
                                .unwrap_or_default()
                        } else {
                            slint::Image::default()
                        };
                    InlineSpanData {
                        text: SharedString::from(sp.text),
                        bold: sp.bold,
                        italic: sp.italic,
                        code: sp.code,
                        strikethrough: sp.strikethrough,
                        math: sp.math,
                        link_url: SharedString::from(sp.link_url),
                        math_image,
                    }
                })
                .collect();
            RichLineData {
                spans: ModelRc::new(VecModel::from(slint_spans)),
            }
        })
        .collect()
}

fn build_rich_lines(
    text: &str,
    max_cols: usize,
    dark_mode: bool,
) -> (ModelRc<RichLineData>, usize) {
    let wrapped = layout_inline_runs_into_lines(text, max_cols);
    let rows = convert_spans_to_rich_lines(wrapped, dark_mode);
    let line_count = rows.len().max(1);
    (ModelRc::new(VecModel::from(rows)), line_count)
}

/// Incremental line-level builder for `ActiveTail` so streaming a long paragraph only
/// re-tokenizes the active last line (Fix F2).
fn build_active_tail_rich_lines(
    msg_index: usize,
    text: &str,
    max_cols: usize,
    dark_mode: bool,
    cache: &mut ActiveTailLineCache,
) -> (ModelRc<RichLineData>, usize) {
    if let Some(last_nl) = text.rfind('\n') {
        let (prefix, tail_line) = text.split_at(last_nl);
        let tail_line = &tail_line[1..]; // skip '\n'
        let cache_valid = cache.msg_index == msg_index
            && cache.max_cols == max_cols
            && cache.dark_mode == dark_mode;

        if cache_valid && cache.stable_prefix == prefix {
            let mut rows = cache.stable_lines.clone();
            let tail_wrapped = layout_inline_runs_into_lines(tail_line, max_cols);
            rows.extend(convert_spans_to_rich_lines(tail_wrapped, dark_mode));
            let line_count = rows.len().max(1);
            return (ModelRc::new(VecModel::from(rows)), line_count);
        }

        if cache_valid
            && prefix.starts_with(&cache.stable_prefix)
            && prefix.as_bytes().get(cache.stable_prefix.len()) == Some(&b'\n')
        {
            let newly_stabilized = &prefix[cache.stable_prefix.len() + 1..];
            let delta_wrapped = layout_inline_runs_into_lines(newly_stabilized, max_cols);
            cache
                .stable_lines
                .extend(convert_spans_to_rich_lines(delta_wrapped, dark_mode));
            cache.stable_prefix = prefix.to_string();

            let mut rows = cache.stable_lines.clone();
            let tail_wrapped = layout_inline_runs_into_lines(tail_line, max_cols);
            rows.extend(convert_spans_to_rich_lines(tail_wrapped, dark_mode));
            let line_count = rows.len().max(1);
            return (ModelRc::new(VecModel::from(rows)), line_count);
        }

        let prefix_wrapped = layout_inline_runs_into_lines(prefix, max_cols);
        let stable_lines = convert_spans_to_rich_lines(prefix_wrapped, dark_mode);
        cache.msg_index = msg_index;
        cache.max_cols = max_cols;
        cache.dark_mode = dark_mode;
        cache.stable_prefix = prefix.to_string();
        cache.stable_lines = stable_lines.clone();

        let mut rows = stable_lines;
        let tail_wrapped = layout_inline_runs_into_lines(tail_line, max_cols);
        rows.extend(convert_spans_to_rich_lines(tail_wrapped, dark_mode));
        let line_count = rows.len().max(1);
        (ModelRc::new(VecModel::from(rows)), line_count)
    } else {
        build_rich_lines(text, max_cols, dark_mode)
    }
}

fn estimate_stream_block_height(b: &StreamBlock, max_cols: usize) -> (f32, bool, Option<String>) {
    let mut is_table = false;
    let mut plain_code = None;
    let est_height = match &b.kind {
        BlockKind::Paragraph | BlockKind::ActiveTail { .. } => {
            let line_cnt: usize = b
                .text
                .split('\n')
                .map(|l| visual_char_width(l).max(1).div_ceil(max_cols.max(1)))
                .sum::<usize>()
                .max(1);
            (line_cnt as f32) * 23.0 + 18.0
        }
        BlockKind::Heading(level) => {
            let (cols, row_h) = match level {
                1 => (max_cols.saturating_sub(20).max(24), 30.0),
                2 => (max_cols.saturating_sub(14).max(28), 26.0),
                _ => (max_cols.saturating_sub(8).max(32), 24.0),
            };
            let line_cnt = visual_char_width(&b.text).max(1).div_ceil(cols);
            (line_cnt as f32) * row_h + 20.0
        }
        BlockKind::ThematicBreak => 30.0,
        BlockKind::Blockquote { lines } => {
            let cols = max_cols.saturating_sub(6).max(20);
            let line_cnt: usize = lines
                .iter()
                .map(|l| visual_char_width(l).max(1).div_ceil(cols))
                .sum::<usize>()
                .max(1);
            (line_cnt as f32) * 22.0 + 34.0
        }
        BlockKind::List { items } => {
            let mut total_lines = 0usize;
            for item in items {
                let avail_cols = max_cols
                    .saturating_sub((item.indent_level as usize) * 4 + 4)
                    .max(20);
                let l_cnt: usize = item
                    .text
                    .split('\n')
                    .map(|l| visual_char_width(l).max(1).div_ceil(avail_cols))
                    .sum::<usize>()
                    .max(1);
                total_lines += l_cnt;
            }
            (total_lines as f32) * 23.0 + (items.len() as f32) * 6.0 + 18.0
        }
        BlockKind::Table { rows, .. } => {
            is_table = true;
            32.0 + 42.0 + (rows.len() as f32) * 34.0 + 20.0
        }
        BlockKind::CodeBlock { code, .. } => {
            plain_code = Some(code.clone());
            let line_cnt = code.lines().count().max(1);
            38.0 + (line_cnt as f32) * 19.5 + 34.0
        }
        BlockKind::Diagram { code, .. } => {
            plain_code = Some(code.clone());
            let step_cnt = code.lines().count().max(1);
            42.0 + (step_cnt as f32) * 44.0 + 38.0
        }
        BlockKind::LatexMath { .. } => 64.0,
        BlockKind::CustomTag { .. } => 48.0,
        BlockKind::ActionCapsule { is_expanded, .. } => {
            if *is_expanded {
                150.0
            } else {
                48.0
            }
        }
    };
    (est_height, is_table, plain_code)
}

fn compile_stream_block_to_render_data(
    msg_index: usize,
    b: &StreamBlock,
    dark_mode: bool,
    max_cols: usize,
    tail_cache: Option<&mut ActiveTailLineCache>,
) -> (RenderBlockData, f32, bool) {
    let mut data = RenderBlockData {
        flat_index: 0,
        msg_index: msg_index as i32,
        block_index: 0,
        id: SharedString::from(&b.id),
        block_type: BlockType::Paragraph,
        text: SharedString::from(&b.text),
        lang: SharedString::default(),
        rich_lines: ModelRc::default(),
        list_items: ModelRc::default(),
        table_headers: ModelRc::default(),
        table_aligns: ModelRc::default(),
        table_rows: ModelRc::default(),
        code_lines: ModelRc::default(),
        max_code_cols: 0,
        diagram_steps: ModelRc::default(),
        math_image: slint::Image::default(),
        detail_text: SharedString::default(),
        is_expanded: false,
        selected: false,
        is_first_in_msg: false,
    };

    let mut is_table = false;
    let est_height: f32;

    match &b.kind {
        BlockKind::Paragraph => {
            data.block_type = BlockType::Paragraph;
            let (rich_lines, line_cnt) = build_rich_lines(&b.text, max_cols, dark_mode);
            data.rich_lines = rich_lines;
            est_height = (line_cnt as f32) * 23.0 + 18.0;
        }
        BlockKind::Heading(level) => {
            let (btype, cols, row_h) = match level {
                1 => (
                    BlockType::Heading1,
                    max_cols.saturating_sub(20).max(24),
                    30.0,
                ),
                2 => (
                    BlockType::Heading2,
                    max_cols.saturating_sub(14).max(28),
                    26.0,
                ),
                _ => (
                    BlockType::Heading3,
                    max_cols.saturating_sub(8).max(32),
                    24.0,
                ),
            };
            data.block_type = btype;
            let (rich_lines, line_cnt) = build_rich_lines(&b.text, cols, dark_mode);
            data.rich_lines = rich_lines;
            est_height = (line_cnt as f32) * row_h + 20.0;
        }
        BlockKind::ThematicBreak => {
            data.block_type = BlockType::ThematicBreak;
            est_height = 30.0;
        }
        BlockKind::Blockquote { lines } => {
            data.block_type = BlockType::Blockquote;
            let joined = lines.join("\n");
            let (rich_lines, line_cnt) =
                build_rich_lines(&joined, max_cols.saturating_sub(6).max(20), dark_mode);
            data.rich_lines = rich_lines;
            est_height = (line_cnt as f32) * 22.0 + 34.0;
        }
        BlockKind::List { items } => {
            data.block_type = BlockType::ListBlock;
            let mut slint_items = Vec::with_capacity(items.len());
            let mut total_lines = 0usize;
            for item in items {
                let avail_cols = max_cols
                    .saturating_sub((item.indent_level as usize) * 4 + 4)
                    .max(20);
                let (rich_lines, line_cnt) = build_rich_lines(&item.text, avail_cols, dark_mode);
                total_lines += line_cnt;
                slint_items.push(ListItemData {
                    indent_level: item.indent_level as i32,
                    marker: SharedString::from(&item.marker),
                    task_state: item.task_state as i32,
                    lines: rich_lines,
                });
            }
            data.list_items = ModelRc::new(VecModel::from(slint_items));
            est_height = (total_lines as f32) * 23.0 + (items.len() as f32) * 6.0 + 18.0;
        }
        BlockKind::Table {
            headers,
            aligns,
            rows,
        } => {
            is_table = true;
            data.block_type = BlockType::TableBlock;
            // Fix L7: strip inline markers on headers just like body cells
            let slint_headers: Vec<SharedString> = headers
                .iter()
                .map(|h| SharedString::from(markdown_parser::strip_inline_markers(h)))
                .collect();
            // Fix U3: propagate GFM column alignments (0=Left, 1=Center, 2=Right)
            let slint_aligns: Vec<i32> = aligns.iter().map(|&a| a as i32).collect();
            let slint_rows: Vec<TableRowData> = rows
                .iter()
                .map(|r| {
                    let cells: Vec<SharedString> = r
                        .iter()
                        .map(|c| SharedString::from(markdown_parser::strip_inline_markers(c)))
                        .collect();
                    TableRowData {
                        cells: ModelRc::new(VecModel::from(cells)),
                    }
                })
                .collect();
            data.table_headers = ModelRc::new(VecModel::from(slint_headers));
            data.table_aligns = ModelRc::new(VecModel::from(slint_aligns));
            data.table_rows = ModelRc::new(VecModel::from(slint_rows));
            est_height = 32.0 + 42.0 + (rows.len() as f32) * 34.0 + 20.0;
        }
        BlockKind::CodeBlock { lang, code, closed } => {
            data.lang = SharedString::from(lang);
            data.text = SharedString::from(code);
            if *closed {
                data.block_type = BlockType::CodeBlock;
                let lines_data = highlighter::highlight_code(code, lang, dark_mode);
                let line_cnt = lines_data.len().max(1);
                let mut max_code_cols = 0usize;
                let line_models: Vec<CodeLineData> = lines_data
                    .into_iter()
                    .enumerate()
                    .map(|(line_idx, l)| {
                        max_code_cols = max_code_cols.max(l.total_cols);
                        let tokens: Vec<HighlightToken> = l
                            .tokens
                            .into_iter()
                            .map(|t| HighlightToken {
                                text: SharedString::from(t.text),
                                color: Color::from_rgb_u8(
                                    t.color_rgb[0],
                                    t.color_rgb[1],
                                    t.color_rgb[2],
                                ),
                            })
                            .collect();
                        CodeLineData {
                            line_number: (line_idx + 1) as i32,
                            tokens: ModelRc::new(VecModel::from(tokens)),
                        }
                    })
                    .collect();
                data.code_lines = ModelRc::new(VecModel::from(line_models));
                data.max_code_cols = max_code_cols as i32;
                est_height = 38.0 + (line_cnt as f32) * 19.5 + 34.0;
            } else {
                data.block_type = BlockType::ActiveCodeTail;
                let line_cnt = code.lines().count().max(1);
                est_height = 38.0 + (line_cnt as f32) * 19.5 + 34.0;
            }
        }
        BlockKind::Diagram { lang: _, code } => {
            data.block_type = BlockType::DiagramBlock;
            let (title, steps) = parse_mermaid_diagram(code);
            data.lang = SharedString::from(title);
            data.text = SharedString::from(code);
            let step_cnt = steps.len().max(1);
            let slint_steps: Vec<DiagramStepData> = steps
                .into_iter()
                .map(|st| DiagramStepData {
                    from_label: SharedString::from(st.from_label),
                    edge_label: SharedString::from(st.edge_label),
                    to_label: SharedString::from(st.to_label),
                })
                .collect();
            data.diagram_steps = ModelRc::new(VecModel::from(slint_steps));
            est_height = 42.0 + (step_cnt as f32) * 44.0 + 38.0;
        }
        BlockKind::LatexMath { raw, closed } => {
            if *closed {
                data.block_type = BlockType::LatexMath;
                if let Some(img) = latex_renderer::render_latex_to_image(raw, dark_mode) {
                    let h = img.size().height as f32;
                    data.math_image = img;
                    est_height = h.max(36.0) + 28.0;
                } else {
                    data.text = SharedString::from(latex_renderer::inline_latex_to_unicode(raw));
                    est_height = 54.0;
                }
            } else {
                data.block_type = BlockType::ActiveTail;
                let (rich_lines, line_cnt) = build_rich_lines(raw, max_cols, dark_mode);
                data.rich_lines = rich_lines;
                est_height = (line_cnt as f32) * 23.0 + 18.0;
            }
        }
        BlockKind::CustomTag { tag_name, content } => {
            data.block_type = BlockType::ActionCapsule;
            data.text = SharedString::from(format!("<{}>", tag_name));
            data.detail_text = SharedString::from(content);
            data.is_expanded = false;
            est_height = 48.0;
        }
        BlockKind::ActionCapsule {
            summary,
            detail_json,
            is_expanded,
        } => {
            data.block_type = BlockType::ActionCapsule;
            data.text = SharedString::from(summary);
            data.detail_text = SharedString::from(detail_json);
            data.is_expanded = *is_expanded;
            est_height = if *is_expanded { 150.0 } else { 48.0 };
        }
        BlockKind::ActiveTail { .. } => {
            data.block_type = BlockType::ActiveTail;
            let (rich_lines, line_cnt) = if let Some(cache) = tail_cache {
                build_active_tail_rich_lines(msg_index, &b.text, max_cols, dark_mode, cache)
            } else {
                build_rich_lines(&b.text, max_cols, dark_mode)
            };
            data.rich_lines = rich_lines;
            est_height = (line_cnt as f32) * 23.0 + 18.0;
        }
    }

    (data, est_height, is_table)
}

fn make_user_block_entry(
    flat_idx: usize,
    msg_idx: usize,
    raw_text: &str,
    max_cols: usize,
    selected: bool,
    measured_heights: &HashMap<(usize, String), f32>,
) -> (FlatBlockEntry, f32) {
    let block_id = format!("user-{}", msg_idx);
    // Fix L10: Accurate wrapped line count without double-counting lines().count()
    let wrap_cols = max_cols.saturating_sub(16).max(20);
    let line_cnt: usize = raw_text
        .split('\n')
        .map(|l| visual_char_width(l).max(1).div_ceil(wrap_cols))
        .sum::<usize>()
        .max(1);
    let est_h = measured_heights
        .get(&(msg_idx, block_id.clone()))
        .copied()
        .unwrap_or((line_cnt as f32) * 21.0 + 46.0);

    let data = RenderBlockData {
        flat_index: flat_idx as i32,
        msg_index: msg_idx as i32,
        block_index: 0,
        id: SharedString::from(&block_id),
        block_type: BlockType::UserBubble,
        text: SharedString::from(raw_text),
        lang: SharedString::default(),
        rich_lines: ModelRc::default(),
        list_items: ModelRc::default(),
        table_headers: ModelRc::default(),
        table_aligns: ModelRc::default(),
        table_rows: ModelRc::default(),
        code_lines: ModelRc::default(),
        max_code_cols: 0,
        diagram_steps: ModelRc::default(),
        math_image: slint::Image::default(),
        detail_text: SharedString::default(),
        is_expanded: false,
        selected,
        is_first_in_msg: true,
    };

    (
        FlatBlockEntry {
            flat_index: flat_idx,
            msg_index: msg_idx,
            block_index: 0,
            block_id,
            is_user: true,
            is_active_tail: false,
            raw_markdown: raw_text.to_string(),
            plain_code_or_text: None,
            is_table: false,
            is_first_in_msg: true,
            source_block: None,
            render_data: Some(data),
        },
        est_h,
    )
}

fn sync_and_cull_ui(window: &MainWindow, records: &[MessageRecord], dark_mode: bool) {
    UI_VIEWPORT_STATE.with(|cell| {
        let mut vp_state = cell.borrow_mut();
        vp_state.dark_mode = dark_mode;
        let max_cols = vp_state.max_cols;

        // Check if any message automaton reset its generation (Fix X6)
        let mut can_incremental =
            !records.is_empty() && vp_state.synced_msg_count == records.len();
        for (msg_idx, rec) in records.iter().enumerate() {
            if !rec.is_user {
                let gen = rec.automaton.read().generation();
                if vp_state.msg_generations.get(&msg_idx).copied() != Some(gen) {
                    vp_state.frozen_cache.retain(|(m, _), _| *m != msg_idx);
                    vp_state.measured_heights.retain(|(m, _), _| *m != msg_idx);
                    vp_state.msg_generations.insert(msg_idx, gen);
                    can_incremental = false;
                }
            }
        }

        // Fix F3: Fast-path incremental update when only the last assistant message is streaming
        if can_incremental {
            let last_msg_idx = records.len() - 1;
            let last_rec = &records[last_msg_idx];
            if !last_rec.is_user {
                let mut auto = last_rec.automaton.write();
                auto.advance(&last_rec.accumulated_content, !last_rec.is_streaming);
                let closed = auto.closed_blocks();
                let tail = auto.active_tail();

                if closed.len() >= vp_state.synced_last_msg_closed {
                    if vp_state.had_active_tail {
                        vp_state.flat_blocks.pop();
                    }

                    for (b_idx, b) in closed
                        .iter()
                        .enumerate()
                        .skip(vp_state.synced_last_msg_closed)
                    {
                        let flat_idx = vp_state.flat_blocks.len();
                        let cache_key = (last_msg_idx, b.id.clone());
                        let (est_h, is_table, plain_code) =
                            estimate_stream_block_height(b, max_cols);
                        let height = vp_state
                            .measured_heights
                            .get(&cache_key)
                            .copied()
                            .unwrap_or(est_h);

                        vp_state.flat_blocks.push(FlatBlockEntry {
                            flat_index: flat_idx,
                            msg_index: last_msg_idx,
                            block_index: b_idx,
                            block_id: b.id.clone(),
                            is_user: false,
                            is_active_tail: false,
                            raw_markdown: b.raw_markdown.clone(),
                            plain_code_or_text: plain_code,
                            is_table,
                            is_first_in_msg: b_idx == 0,
                            source_block: Some(b.clone()),
                            render_data: None,
                        });
                        if flat_idx < vp_state.fenwick.len() {
                            vp_state.fenwick.update_height(flat_idx, height);
                        } else {
                            vp_state.fenwick.push_height(height);
                        }
                    }

                    vp_state.synced_last_msg_closed = closed.len();

                    if let Some(t) = tail {
                        let flat_idx = vp_state.flat_blocks.len();
                        let b_idx = closed.len();
                        let (est_h, is_table, plain_code) =
                            estimate_stream_block_height(t, max_cols);
                        vp_state.flat_blocks.push(FlatBlockEntry {
                            flat_index: flat_idx,
                            msg_index: last_msg_idx,
                            block_index: b_idx,
                            block_id: t.id.clone(),
                            is_user: false,
                            is_active_tail: true,
                            raw_markdown: t.raw_markdown.clone(),
                            plain_code_or_text: plain_code,
                            is_table,
                            is_first_in_msg: b_idx == 0,
                            source_block: Some(t.clone()),
                            render_data: None,
                        });
                        if flat_idx < vp_state.fenwick.len() {
                            vp_state.fenwick.update_height(flat_idx, est_h);
                        } else {
                            vp_state.fenwick.push_height(est_h);
                        }
                        vp_state.had_active_tail = true;
                    } else {
                        vp_state.had_active_tail = false;
                        // If fenwick had one extra slot from the popped active_tail, resync heights
                        if vp_state.fenwick.len() != vp_state.flat_blocks.len() {
                            let heights: Vec<f32> = (0..vp_state.flat_blocks.len())
                                .map(|i| vp_state.fenwick.get_height(i))
                                .collect();
                            vp_state.fenwick.sync_heights(&heights);
                        }
                    }

                    drop(auto);
                    let total_len = vp_state.flat_blocks.len();
                    vp_state.selected_indices.retain(|&idx| idx < total_len);
                    apply_viewport_slice(window, &mut vp_state);
                    return;
                }
            }
        }

        // Full structural pass (lazy compilation: only metadata & heights are collected here;
        // actual Slint RenderBlockData models are compiled on demand in `apply_viewport_slice`)
        let mut flat_blocks = Vec::new();
        let mut heights = Vec::new();
        let mut last_closed_cnt = 0usize;
        let mut last_had_tail = false;

        for (msg_idx, rec) in records.iter().enumerate() {
            if rec.is_user {
                let flat_idx = flat_blocks.len();
                let selected = vp_state.selected_indices.contains(&flat_idx);
                let (entry, est_h) = make_user_block_entry(
                    flat_idx,
                    msg_idx,
                    &rec.accumulated_content,
                    max_cols,
                    selected,
                    &vp_state.measured_heights,
                );
                flat_blocks.push(entry);
                heights.push(est_h);
                continue;
            }

            // Fix F1: zero-copy `advance` + borrowed slices
            let mut auto = rec.automaton.write();
            auto.advance(&rec.accumulated_content, !rec.is_streaming);
            let closed = auto.closed_blocks();
            let tail = auto.active_tail();

            for (b_idx, b) in closed.iter().enumerate() {
                let flat_idx = flat_blocks.len();
                let cache_key = (msg_idx, b.id.clone());
                let (est_h, is_table, plain_code) = estimate_stream_block_height(b, max_cols);
                let height = vp_state
                    .measured_heights
                    .get(&cache_key)
                    .copied()
                    .unwrap_or_else(|| {
                        vp_state
                            .frozen_cache
                            .get(&cache_key)
                            .map(|c| c.estimated_height)
                            .unwrap_or(est_h)
                    });

                flat_blocks.push(FlatBlockEntry {
                    flat_index: flat_idx,
                    msg_index: msg_idx,
                    block_index: b_idx,
                    block_id: b.id.clone(),
                    is_user: false,
                    is_active_tail: false,
                    raw_markdown: b.raw_markdown.clone(),
                    plain_code_or_text: plain_code,
                    is_table,
                    is_first_in_msg: b_idx == 0,
                    source_block: Some(b.clone()),
                    render_data: None,
                });
                heights.push(height);
            }

            if msg_idx + 1 == records.len() {
                last_closed_cnt = closed.len();
                last_had_tail = tail.is_some();
            }

            if let Some(t) = tail {
                let flat_idx = flat_blocks.len();
                let b_idx = closed.len();
                let (est_h, is_table, plain_code) = estimate_stream_block_height(t, max_cols);

                flat_blocks.push(FlatBlockEntry {
                    flat_index: flat_idx,
                    msg_index: msg_idx,
                    block_index: b_idx,
                    block_id: t.id.clone(),
                    is_user: false,
                    is_active_tail: true,
                    raw_markdown: t.raw_markdown.clone(),
                    plain_code_or_text: plain_code,
                    is_table,
                    is_first_in_msg: b_idx == 0,
                    source_block: Some(t.clone()),
                    render_data: None,
                });
                heights.push(est_h);
            }
        }

        vp_state.fenwick.sync_heights(&heights);
        vp_state.flat_blocks = flat_blocks;
        vp_state.synced_msg_count = records.len();
        vp_state.synced_last_msg_closed = last_closed_cnt;
        vp_state.had_active_tail = last_had_tail;
        let total_len = vp_state.flat_blocks.len();
        vp_state.selected_indices.retain(|&idx| idx < total_len);
        apply_viewport_slice(window, &mut vp_state);
    });
}

/// Lazily compiles ONLY the blocks inside `[slice.start_idx .. slice.end_idx)` (Fix F4 & L2).
fn apply_viewport_slice(window: &MainWindow, vp_state: &mut UiViewportState) {
    let slice = vp_state.fenwick.query_viewport(
        vp_state.scroll_y,
        vp_state.viewport_height,
        VIEWPORT_OVERSCAN_PX,
    );

    let dark_mode = vp_state.dark_mode;
    let max_cols = vp_state.max_cols;
    let mut visible = Vec::with_capacity(slice.end_idx.saturating_sub(slice.start_idx));

    for idx in slice.start_idx..slice.end_idx {
        let selected = vp_state.selected_indices.contains(&idx);
        let entry = &vp_state.flat_blocks[idx];

        if entry.is_user {
            if let Some(ref rdata) = entry.render_data {
                let mut d = rdata.clone();
                d.flat_index = idx as i32;
                d.selected = selected;
                visible.push(d);
            }
            continue;
        }

        let msg_idx = entry.msg_index;
        let b_idx = entry.block_index;
        let is_first = entry.is_first_in_msg;
        let is_tail = entry.is_active_tail;
        let block_id = entry.block_id.clone();

        if is_tail {
            if let Some(sb) = entry.source_block.clone() {
                let (mut rdata, est_h, _) = compile_stream_block_to_render_data(
                    msg_idx,
                    &sb,
                    dark_mode,
                    max_cols,
                    Some(&mut vp_state.active_tail_cache),
                );
                rdata.flat_index = idx as i32;
                rdata.msg_index = msg_idx as i32;
                rdata.block_index = b_idx as i32;
                rdata.selected = selected;
                rdata.is_first_in_msg = is_first;
                if !vp_state.measured_heights.contains_key(&(msg_idx, block_id)) {
                    vp_state.fenwick.update_height(idx, est_h);
                }
                visible.push(rdata);
            }
            continue;
        }

        // Closed block: check `frozen_cache` for matching `(dark_mode, max_cols, is_expanded)`
        let cache_key = (msg_idx, block_id.clone());
        let sb_opt = entry.source_block.clone();
        let wanted_expanded = match sb_opt.as_ref().map(|s| &s.kind) {
            Some(BlockKind::ActionCapsule { is_expanded, .. }) => *is_expanded,
            _ => false,
        };

        let needs_compile = match vp_state.frozen_cache.get(&cache_key) {
            Some(cached) => {
                cached.dark_mode != dark_mode
                    || cached.max_cols != max_cols
                    || cached.is_expanded != wanted_expanded
            }
            None => true,
        };

        if needs_compile {
            if let Some(ref sb) = sb_opt {
                let (compiled, est_h, _) =
                    compile_stream_block_to_render_data(msg_idx, sb, dark_mode, max_cols, None);
                vp_state.frozen_cache.insert(
                    cache_key.clone(),
                    CachedFrozenBlock {
                        render_data: compiled,
                        estimated_height: est_h,
                        dark_mode,
                        max_cols,
                        is_expanded: wanted_expanded,
                    },
                );
                if !vp_state.measured_heights.contains_key(&cache_key) {
                    vp_state.fenwick.update_height(idx, est_h);
                }
            }
        }

        if let Some(cached) = vp_state.frozen_cache.get(&cache_key) {
            let mut d = cached.render_data.clone();
            d.flat_index = idx as i32;
            d.msg_index = msg_idx as i32;
            d.block_index = b_idx as i32;
            d.selected = selected;
            d.is_first_in_msg = is_first;
            visible.push(d);
        }
    }

    // Re-read spacer heights in case lazy compilation updated an unmeasured block height
    let updated_slice = vp_state.fenwick.query_viewport(
        vp_state.scroll_y,
        vp_state.viewport_height,
        VIEWPORT_OVERSCAN_PX,
    );

    let state = window.global::<AppState>();
    state.set_top_spacer_height(updated_slice.top_spacer_height);
    state.set_bottom_spacer_height(updated_slice.bottom_spacer_height);
    state.set_selected_block_count(vp_state.selected_indices.len() as i32);
    window.set_visible_blocks(ModelRc::new(VecModel::from(visible)));
}

fn serialize_single_block(entry: &FlatBlockEntry, mode: &str) -> String {
    let raw = entry.raw_markdown.as_str();
    match mode {
        "copy_markdown" => raw.to_string(),
        "copy_tsv" => {
            if entry.is_table {
                table_markdown_to_tsv(raw)
            } else {
                markdown_to_plain_text(raw)
            }
        }
        // Fix L8: "copy_plain" separates table plain text from TSV and preserves raw code
        _ => {
            if entry.is_table {
                table_markdown_to_plain_text(raw)
            } else if let Some(ref code) = entry.plain_code_or_text {
                code.clone()
            } else {
                markdown_to_plain_text(raw)
            }
        }
    }
}

fn serialize_blocks_for_clipboard(vp_state: &UiViewportState, mode: &str) -> String {
    let target_indices: Vec<usize> = if !vp_state.selected_indices.is_empty() {
        vp_state.selected_indices.iter().copied().collect()
    } else if let Some(idx) = vp_state.context_target_flat {
        vec![idx]
    } else {
        Vec::new()
    };

    let mut chunks = Vec::new();
    for idx in target_indices {
        if let Some(entry) = vp_state.flat_blocks.get(idx) {
            let chunk = serialize_single_block(entry, mode);
            if !chunk.is_empty() {
                chunks.push(chunk);
            }
        }
    }
    chunks.join("\n\n")
}

fn main() -> Result<(), slint::PlatformError> {
    if std::env::var_os("SLINT_BACKEND").is_none() {
        std::env::set_var("SLINT_BACKEND", "winit-software");
    }

    let app = MainWindow::new()?;

    // 1. Initialize Multi-Session Sandbox Store at `C:\Users\<Username>\.fugue`
    let session_store = Arc::new(
        SessionSandboxStore::open_default()
            .expect("Failed to open Fugue session sandbox store at ~/.fugue"),
    );

    // 2. Restore persisted client settings (Theme, Language, Sidebar, Active Session)
    let initial_cfg = session_store.get_config();
    {
        let state = app.global::<AppState>();
        state.set_fugue_root_path(SharedString::from(
            session_store.root_dir().to_string_lossy().to_string(),
        ));
        state.set_sidebar_open(initial_cfg.sidebar_open);
        state.set_lang(SharedString::from(lang_pref_to_str(initial_cfg.lang)));
        state.set_theme_mode(theme_pref_to_slint(initial_cfg.theme));
    }

    let initial_dark = app.global::<Theme>().get_dark();
    UI_VIEWPORT_STATE.with(|cell| {
        cell.borrow_mut().dark_mode = initial_dark;
    });

    // 3. Initialize Fugue Living Node Memory & Omni Facade (AXIOM[02])
    let memory = Arc::new(LivingNodeMemory::new());
    let omni = Arc::new(OmniNode::new(memory.clone()));

    // 4. Thread-safe message state & stop signal
    let records: Arc<RwLock<Vec<MessageRecord>>> = Arc::new(RwLock::new(Vec::new()));
    let stop_signal = Arc::new(AtomicBool::new(false));

    // 5. Initialize Reserved LLM Client Control Gateway (with UI synchronization hook)
    let gateway_ui_hook = {
        let app_weak = app.as_weak();
        let store_for_hook = session_store.clone();
        let records_for_hook = records.clone();
        Arc::new(move |resp: &ClientControlResponse| {
            let resp_clone = resp.clone();
            let weak = app_weak.clone();
            let store = store_for_hook.clone();
            let recs = records_for_hook.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(handle) = weak.upgrade() else {
                    return;
                };
                match resp_clone {
                    ClientControlResponse::SettingsUpdated { theme, lang } => {
                        let state = handle.global::<AppState>();
                        state.set_theme_mode(theme_pref_to_slint(theme));
                        state.set_lang(SharedString::from(lang_pref_to_str(lang)));
                        let is_dark = handle.global::<Theme>().get_dark();
                        UI_VIEWPORT_STATE.with(|cell| {
                            let mut vp = cell.borrow_mut();
                            if vp.dark_mode != is_dark {
                                vp.dark_mode = is_dark;
                                apply_viewport_slice(&handle, &mut vp);
                            }
                        });
                        refresh_session_list_ui(&handle, &store);
                    }
                    ClientControlResponse::SessionDeleted { deleted_session_id } => {
                        let cur_sid = handle.global::<AppState>().get_current_session_id();
                        if cur_sid.as_str() == deleted_session_id.to_string().as_str() {
                            let next_id = store
                                .list_sessions(false)
                                .first()
                                .map(|m| m.id)
                                .or_else(|| store.list_sessions(true).first().map(|m| m.id))
                                .unwrap_or_else(|| {
                                    store.create_session(None).expect("create fallback").id
                                });
                            load_session_into_ui(&handle, &store, &recs, next_id);
                        } else {
                            refresh_session_list_ui(&handle, &store);
                        }
                    }
                    ClientControlResponse::SessionCreatedOrUpdated { .. } => {
                        refresh_session_list_ui(&handle, &store);
                    }
                    _ => {}
                }
            });
        })
    };
    let control_gateway = ClientControlGateway::new(session_store.clone(), Some(gateway_ui_hook));

    // Load initial active session into UI
    let active_sid = initial_cfg
        .active_session_id
        .filter(|&id| session_store.get_session_metadata(id).is_ok())
        .or_else(|| session_store.list_sessions(false).first().map(|m| m.id))
        .or_else(|| session_store.list_sessions(true).first().map(|m| m.id))
        .unwrap_or_else(|| {
            session_store
                .create_session(None)
                .expect("Failed to create initial session")
                .id
        });
    load_session_into_ui(&app, &session_store, &records, active_sid);

    // 6. Tokio background runtime
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("Failed to initialize Tokio runtime");

    // 7. Connect UI Callbacks
    // 7.1 Copy to clipboard
    app.global::<AppState>().on_copy_to_clipboard({
        move |text| {
            if let Ok(mut clipboard) = arboard::Clipboard::new() {
                let _ = clipboard.set_text(text.to_string());
            }
        }
    });

    // 7.1b Table / Block formatted copy button (Fix U7 & M8)
    app.global::<AppState>().on_copy_block_formatted({
        move |flat_idx, mode| {
            let idx = flat_idx.max(0) as usize;
            UI_VIEWPORT_STATE.with(|cell| {
                let vp = cell.borrow();
                if let Some(entry) = vp.flat_blocks.get(idx) {
                    let payload = serialize_single_block(entry, mode.as_str());
                    if !payload.is_empty() {
                        if let Ok(mut clipboard) = arboard::Clipboard::new() {
                            let _ = clipboard.set_text(payload);
                        }
                    }
                }
            });
        }
    });

    // 7.1c Theme changed callback: lazily re-renders visible blocks in new theme (Fix L2 & F4)
    app.global::<AppState>().on_theme_changed({
        let app_weak = app.as_weak();
        move |_theme_mode| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            let is_dark = handle.global::<Theme>().get_dark();
            UI_VIEWPORT_STATE.with(|cell| {
                let mut vp = cell.borrow_mut();
                if vp.dark_mode != is_dark {
                    vp.dark_mode = is_dark;
                    apply_viewport_slice(&handle, &mut vp);
                }
            });
        }
    });

    // 7.1d Theme mode & Language persistence (`C:\Users\<Username>\.fugue\config.rkyv`)
    app.global::<AppState>().on_theme_mode_selected({
        let store = session_store.clone();
        move |mode| {
            let pref = slint_to_theme_pref(mode);
            let _ = store.update_config(|c| c.theme = pref);
        }
    });

    app.global::<AppState>().on_language_changed({
        let store = session_store.clone();
        let app_weak = app.as_weak();
        move |lang_str| {
            let pref = str_to_lang_pref(lang_str.as_str());
            let _ = store.update_config(|c| c.lang = pref);
            if let Some(handle) = app_weak.upgrade() {
                refresh_session_list_ui(&handle, &store);
            }
        }
    });

    // 7.1e Measured block height feedback from Slint (`init` & `changed height`) (Fix L3, U11, X7)
    app.global::<AppState>().on_report_block_height({
        let app_weak = app.as_weak();
        move |flat_idx, measured_px| {
            if measured_px <= 1.0 {
                return;
            }
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            let idx = flat_idx.max(0) as usize;
            UI_VIEWPORT_STATE.with(|cell| {
                let mut vp = cell.borrow_mut();
                if idx >= vp.flat_blocks.len() || idx >= vp.fenwick.len() {
                    return;
                }
                let old_h = vp.fenwick.get_height(idx);
                if (old_h - measured_px).abs() <= 0.5 {
                    return;
                }
                let key = (
                    vp.flat_blocks[idx].msg_index,
                    vp.flat_blocks[idx].block_id.clone(),
                );
                vp.measured_heights.insert(key, measured_px);
                vp.fenwick.update_height(idx, measured_px);

                let slice = vp.fenwick.query_viewport(
                    vp.scroll_y,
                    vp.viewport_height,
                    VIEWPORT_OVERSCAN_PX,
                );
                let state = handle.global::<AppState>();
                state.set_top_spacer_height(slice.top_spacer_height);
                state.set_bottom_spacer_height(slice.bottom_spacer_height);
            });
        }
    });

    // 7.2 Fenwick Tree Viewport Scrolled & Dynamic Column Width Reflow (Fix M4)
    app.global::<AppState>().on_viewport_scrolled({
        let app_weak = app.as_weak();

        move |scroll_y, viewport_h, viewport_w| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            UI_VIEWPORT_STATE.with(|cell| {
                let mut vp = cell.borrow_mut();
                let old_slice = vp.fenwick.query_viewport(
                    vp.scroll_y,
                    vp.viewport_height,
                    VIEWPORT_OVERSCAN_PX,
                );
                vp.scroll_y = scroll_y.max(0.0);
                vp.viewport_height = viewport_h.max(100.0);

                // Fix M4: dynamic inline wrap column budget from viewport width
                let computed_cols = ((viewport_w - 40.0) / 8.2).clamp(28.0, 104.0) as usize;
                let cols_changed = computed_cols.abs_diff(vp.max_cols) >= 3;
                if cols_changed {
                    vp.max_cols = computed_cols;
                }

                let new_slice = vp.fenwick.query_viewport(
                    vp.scroll_y,
                    vp.viewport_height,
                    VIEWPORT_OVERSCAN_PX,
                );
                if cols_changed
                    || old_slice.start_idx != new_slice.start_idx
                    || old_slice.end_idx != new_slice.end_idx
                {
                    apply_viewport_slice(&handle, &mut vp);
                }
            });
        }
    });

    // 7.2b Global Lenis (`duration = 1.2`, `lerp = 0.1`) Smooth Scroll Callbacks
    app.global::<AppState>().on_lenis_wheel_chat({
        let app_weak = app.as_weak();
        move |delta_y_px, max_scroll_px| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            LENIS_STATE.with(|cell| {
                let mut mgr = cell.borrow_mut();
                mgr.chat
                    .push_wheel_delta(delta_y_px as f64, max_scroll_px as f64);
                mgr.touch_clock();
                let state = handle.global::<AppState>();
                if delta_y_px > 0.0 {
                    state.set_auto_scroll_sticky(false);
                } else if delta_y_px < 0.0 && mgr.chat.target >= (max_scroll_px as f64 - 36.0).max(0.0)
                {
                    state.set_auto_scroll_sticky(true);
                }
            });
            ensure_lenis_timer_running(app_weak.clone());
        }
    });

    app.global::<AppState>().on_lenis_sync_chat({
        move |direct_pos_px, max_scroll_px| {
            LENIS_STATE.with(|cell| {
                cell.borrow_mut()
                    .chat
                    .jump_to(direct_pos_px as f64, max_scroll_px as f64);
            });
        }
    });

    app.global::<AppState>().on_lenis_wheel_sidebar({
        let app_weak = app.as_weak();
        move |delta_y_px, max_scroll_px| {
            LENIS_STATE.with(|cell| {
                let mut mgr = cell.borrow_mut();
                mgr.sidebar
                    .push_wheel_delta(delta_y_px as f64, max_scroll_px as f64);
                mgr.touch_clock();
            });
            ensure_lenis_timer_running(app_weak.clone());
        }
    });

    app.global::<AppState>().on_lenis_sync_sidebar({
        move |direct_pos_px, max_scroll_px| {
            LENIS_STATE.with(|cell| {
                cell.borrow_mut()
                    .sidebar
                    .jump_to(direct_pos_px as f64, max_scroll_px as f64);
            });
        }
    });

    app.global::<AppState>().on_lenis_wheel_input({
        let app_weak = app.as_weak();
        move |delta_y_px, max_scroll_px| {
            LENIS_STATE.with(|cell| {
                let mut mgr = cell.borrow_mut();
                mgr.input
                    .push_wheel_delta(delta_y_px as f64, max_scroll_px as f64);
                mgr.touch_clock();
            });
            ensure_lenis_timer_running(app_weak.clone());
        }
    });

    app.global::<AppState>().on_lenis_sync_input({
        move |direct_pos_px, max_scroll_px| {
            LENIS_STATE.with(|cell| {
                cell.borrow_mut()
                    .input
                    .jump_to(direct_pos_px as f64, max_scroll_px as f64);
            });
        }
    });

    // 7.2c Multi-Session Management Callbacks (Sidebar, Create, Select, Clone, Rename, Pin, Archive, Delete, Search)
    app.global::<AppState>().on_toggle_sidebar({
        let app_weak = app.as_weak();
        let store = session_store.clone();
        move || {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            let state = handle.global::<AppState>();
            let next = !state.get_sidebar_open();
            state.set_sidebar_open(next);
            let _ = store.update_config(|c| c.sidebar_open = next);
        }
    });

    app.global::<AppState>().on_create_new_session({
        let app_weak = app.as_weak();
        let store = session_store.clone();
        let records = records.clone();
        let stop_signal = stop_signal.clone();
        move || {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            finalize_active_generation(&handle, &store, &records, &stop_signal);
            handle.global::<AppState>().set_show_archived_sessions(false);
            if let Ok(meta) = store.create_session(None) {
                load_session_into_ui(&handle, &store, &records, meta.id);
            }
        }
    });

    app.global::<AppState>().on_select_session({
        let app_weak = app.as_weak();
        let store = session_store.clone();
        let records = records.clone();
        let stop_signal = stop_signal.clone();
        move |sid_str| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            if handle.global::<AppState>().get_current_session_id() == sid_str {
                return;
            }
            let Ok(sid) = UuidV7::parse_str(sid_str.as_str()) else {
                return;
            };
            finalize_active_generation(&handle, &store, &records, &stop_signal);
            load_session_into_ui(&handle, &store, &records, sid);
        }
    });

    app.global::<AppState>().on_clone_session({
        let app_weak = app.as_weak();
        let store = session_store.clone();
        let records = records.clone();
        let stop_signal = stop_signal.clone();
        move |sid_str| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            let Ok(sid) = UuidV7::parse_str(sid_str.as_str()) else {
                return;
            };
            finalize_active_generation(&handle, &store, &records, &stop_signal);
            if let Ok(child_meta) = store.clone_session(sid, None) {
                handle.global::<AppState>().set_show_archived_sessions(false);
                load_session_into_ui(&handle, &store, &records, child_meta.id);
            }
        }
    });

    app.global::<AppState>().on_rename_session({
        let app_weak = app.as_weak();
        let store = session_store.clone();
        move |sid_str, new_title| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            let Ok(sid) = UuidV7::parse_str(sid_str.as_str()) else {
                return;
            };
            if store.rename_session(sid, new_title.as_str()).is_ok() {
                refresh_session_list_ui(&handle, &store);
            }
        }
    });

    app.global::<AppState>().on_toggle_pin_session({
        let app_weak = app.as_weak();
        let store = session_store.clone();
        move |sid_str, new_pinned| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            let Ok(sid) = UuidV7::parse_str(sid_str.as_str()) else {
                return;
            };
            if store.set_session_pinned(sid, new_pinned).is_ok() {
                refresh_session_list_ui(&handle, &store);
            }
        }
    });

    app.global::<AppState>().on_toggle_archive_session({
        let app_weak = app.as_weak();
        let store = session_store.clone();
        let records = records.clone();
        let stop_signal = stop_signal.clone();
        move |sid_str, new_archived| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            let Ok(sid) = UuidV7::parse_str(sid_str.as_str()) else {
                return;
            };
            if store.set_session_archived(sid, new_archived).is_ok() {
                let state = handle.global::<AppState>();
                if new_archived && state.get_current_session_id() == sid_str {
                    finalize_active_generation(&handle, &store, &records, &stop_signal);
                    let next_id = store
                        .list_sessions(false)
                        .first()
                        .map(|m| m.id)
                        .unwrap_or_else(|| {
                            store
                                .create_session(None)
                                .expect("create fallback session")
                                .id
                        });
                    load_session_into_ui(&handle, &store, &records, next_id);
                } else {
                    refresh_session_list_ui(&handle, &store);
                }
            }
        }
    });

    app.global::<AppState>().on_delete_session({
        let app_weak = app.as_weak();
        let store = session_store.clone();
        let records = records.clone();
        let stop_signal = stop_signal.clone();
        move |sid_str| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            let Ok(sid) = UuidV7::parse_str(sid_str.as_str()) else {
                return;
            };
            let deleting_current = handle.global::<AppState>().get_current_session_id() == sid_str;
            if deleting_current {
                finalize_active_generation(&handle, &store, &records, &stop_signal);
            }
            if store.delete_session(sid).is_ok() {
                if deleting_current {
                    let show_archived = handle.global::<AppState>().get_show_archived_sessions();
                    let next_id = store
                        .list_sessions(show_archived)
                        .first()
                        .map(|m| m.id)
                        .or_else(|| store.list_sessions(false).first().map(|m| m.id))
                        .unwrap_or_else(|| {
                            handle
                                .global::<AppState>()
                                .set_show_archived_sessions(false);
                            store
                                .create_session(None)
                                .expect("create fallback session")
                                .id
                        });
                    load_session_into_ui(&handle, &store, &records, next_id);
                } else {
                    refresh_session_list_ui(&handle, &store);
                }
            }
        }
    });

    app.global::<AppState>().on_switch_archive_tab({
        let app_weak = app.as_weak();
        let store = session_store.clone();
        move |show_archived| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            handle
                .global::<AppState>()
                .set_show_archived_sessions(show_archived);
            refresh_session_list_ui(&handle, &store);
        }
    });

    app.global::<AppState>().on_search_sessions_query({
        let app_weak = app.as_weak();
        let store = session_store.clone();
        move |query| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            refresh_search_hits_ui(&handle, &store, query.as_str());
        }
    });

    app.global::<AppState>().on_jump_to_search_hit({
        let app_weak = app.as_weak();
        let store = session_store.clone();
        let records = records.clone();
        let stop_signal = stop_signal.clone();
        move |sid_str, mid_str, msg_idx_hint| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            let Ok(sid) = UuidV7::parse_str(sid_str.as_str()) else {
                return;
            };
            let target_mid = UuidV7::parse_str(mid_str.as_str()).ok();

            if handle.global::<AppState>().get_current_session_id() != sid_str {
                finalize_active_generation(&handle, &store, &records, &stop_signal);
                load_session_into_ui(&handle, &store, &records, sid);
            }

            // Locate message index by UUIDv7 (or fall back to msg_idx_hint)
            let resolved_msg_idx = {
                let recs = records.read();
                target_mid
                    .and_then(|mid| recs.iter().position(|r| r.message_id == mid))
                    .unwrap_or_else(|| (msg_idx_hint.max(0) as usize).min(recs.len().saturating_sub(1)))
            };

            // Compute exact vertical pixel offset via Fenwick tree prefix sum and highlight target message blocks
            let scroll_target = UI_VIEWPORT_STATE.with(|cell| {
                let mut vp = cell.borrow_mut();
                vp.selected_indices.clear();
                let matching_flats: Vec<usize> = vp
                    .flat_blocks
                    .iter()
                    .filter(|b| b.msg_index == resolved_msg_idx)
                    .map(|b| b.flat_index)
                    .collect();
                let first_flat = matching_flats.first().copied();
                for f_idx in matching_flats {
                    vp.selected_indices.insert(f_idx);
                }
                let target_y = first_flat
                    .map(|f_idx| vp.fenwick.offset_of(f_idx).max(0.0))
                    .unwrap_or(0.0);
                let total_h = vp.fenwick.total_height();
                let max_scroll = (total_h - vp.viewport_height).max(0.0);
                let clamped_y = target_y.clamp(0.0, max_scroll);
                vp.scroll_y = clamped_y;
                apply_viewport_slice(&handle, &mut vp);
                (clamped_y, max_scroll)
            });

            handle.global::<AppState>().set_auto_scroll_sticky(false);
            LENIS_STATE.with(|cell| {
                let mut mgr = cell.borrow_mut();
                mgr.chat
                    .smooth_scroll_to(scroll_target.0 as f64, scroll_target.1 as f64);
                mgr.touch_clock();
            });
            ensure_lenis_timer_running(app_weak.clone());
        }
    });

    // 7.3 Left-click / drag block selection (`agent-block-copy.ts` parity)
    app.global::<AppState>().on_block_left_pressed({
        let app_weak = app.as_weak();

        move |flat_idx, shift, ctrl| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            let idx = flat_idx.max(0) as usize;
            UI_VIEWPORT_STATE.with(|cell| {
                let mut vp = cell.borrow_mut();
                if shift {
                    let anchor = vp.selection_anchor.unwrap_or(idx);
                    let start = anchor.min(idx);
                    let end = anchor.max(idx);
                    vp.selected_indices.clear();
                    for i in start..=end {
                        vp.selected_indices.insert(i);
                    }
                } else if ctrl {
                    if vp.selected_indices.contains(&idx) {
                        vp.selected_indices.remove(&idx);
                    } else {
                        vp.selected_indices.insert(idx);
                    }
                    vp.selection_anchor = Some(idx);
                } else {
                    vp.selected_indices.clear();
                    vp.selected_indices.insert(idx);
                    vp.selection_anchor = Some(idx);
                }
                apply_viewport_slice(&handle, &mut vp);
            });
        }
    });

    app.global::<AppState>().on_block_hover_drag({
        let app_weak = app.as_weak();

        move |flat_idx| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            let idx = flat_idx.max(0) as usize;
            UI_VIEWPORT_STATE.with(|cell| {
                let mut vp = cell.borrow_mut();
                let anchor = vp.selection_anchor.unwrap_or(idx);
                let start = anchor.min(idx);
                let end = anchor.max(idx);
                let new_range: BTreeSet<usize> = (start..=end).collect();
                if vp.selected_indices != new_range {
                    vp.selected_indices = new_range;
                    apply_viewport_slice(&handle, &mut vp);
                }
            });
        }
    });

    // 7.4 Right-click opens Singleton Context Menu without mutating selection
    app.global::<AppState>().on_block_right_clicked({
        let app_weak = app.as_weak();

        move |flat_idx, msg_idx, is_table, win_x, win_y| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            let f_idx = flat_idx.max(0) as usize;
            let m_idx = msg_idx.max(0) as usize;
            UI_VIEWPORT_STATE.with(|cell| {
                let mut vp = cell.borrow_mut();
                vp.context_target_flat = Some(f_idx);
                vp.context_target_msg = Some(m_idx);
            });
            let state = handle.global::<AppState>();
            state.set_context_menu_is_table(is_table);
            state.set_context_menu_x(win_x);
            state.set_context_menu_y(win_y);
            state.set_context_menu_open(true);
        }
    });

    // 7.5 Singleton Context Menu Action Dispatch (Lazy Serialization on Click)
    app.global::<AppState>().on_context_menu_action({
        let app_weak = app.as_weak();

        move |action| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            let act = action.as_str();
            UI_VIEWPORT_STATE.with(|cell| {
                let mut vp = cell.borrow_mut();
                match act {
                    "copy_plain" | "copy_markdown" | "copy_tsv" => {
                        let payload = serialize_blocks_for_clipboard(&vp, act);
                        if !payload.is_empty() {
                            if let Ok(mut clipboard) = arboard::Clipboard::new() {
                                let _ = clipboard.set_text(payload);
                            }
                        }
                    }
                    "select_block" => {
                        if let Some(target) = vp.context_target_flat {
                            vp.selected_indices.clear();
                            vp.selected_indices.insert(target);
                            vp.selection_anchor = Some(target);
                            apply_viewport_slice(&handle, &mut vp);
                        }
                    }
                    "select_message" => {
                        if let Some(target_msg) = vp.context_target_msg {
                            vp.selected_indices.clear();
                            let matching: Vec<usize> = vp
                                .flat_blocks
                                .iter()
                                .filter(|b| b.msg_index == target_msg)
                                .map(|b| b.flat_index)
                                .collect();
                            for idx in matching {
                                vp.selected_indices.insert(idx);
                            }
                            apply_viewport_slice(&handle, &mut vp);
                        }
                    }
                    "select_all" => {
                        vp.selected_indices = (0..vp.flat_blocks.len()).collect();
                        apply_viewport_slice(&handle, &mut vp);
                    }
                    "clear_selection" => {
                        vp.selected_indices.clear();
                        vp.selection_anchor = None;
                        apply_viewport_slice(&handle, &mut vp);
                    }
                    _ => {}
                }
            });
        }
    });

    // 7.6 Toggle Action Capsule ($O(\log N)$ height update + sandbox persistence)
    app.global::<AppState>().on_toggle_capsule({
        let records = records.clone();
        let store = session_store.clone();
        let app_weak = app.as_weak();

        move |flat_idx| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            let idx = flat_idx.max(0) as usize;
            let target_info = UI_VIEWPORT_STATE.with(|cell| {
                let mut vp = cell.borrow_mut();
                if let Some(b) = vp.flat_blocks.get(idx) {
                    let key = (b.msg_index, b.block_id.clone());
                    vp.measured_heights.remove(&key);
                    // Force full structural pass on capsule toggle
                    vp.synced_msg_count = 0;
                    Some(key)
                } else {
                    None
                }
            });

            if let Some((msg_idx, block_id)) = target_info {
                let recs = records.read();
                if let Some(rec) = recs.get(msg_idx) {
                    let updated_capsules = {
                        let mut auto = rec.automaton.write();
                        auto.toggle_capsule(&block_id);
                        auto.extract_capsules()
                    };
                    if rec.persisted {
                        if let Ok(sid) = UuidV7::parse_str(
                            handle.global::<AppState>().get_current_session_id().as_str(),
                        ) {
                            let _ = store.update_message_capsules(sid, msg_idx, updated_capsules);
                        }
                    }
                    let is_dark = handle.global::<Theme>().get_dark();
                    sync_and_cull_ui(&handle, &recs, is_dark);
                }
            }
        }
    });

    // 7.7 Stop generation: immediately finalize active tail into closed blocks & persist to sandbox
    app.global::<AppState>().on_stop_generation({
        let stop_signal = stop_signal.clone();
        let records = records.clone();
        let store = session_store.clone();
        let app_weak = app.as_weak();

        move || {
            if let Some(handle) = app_weak.upgrade() {
                finalize_active_generation(&handle, &store, &records, &stop_signal);
                {
                    let recs = records.read();
                    let is_dark = handle.global::<Theme>().get_dark();
                    sync_and_cull_ui(&handle, &recs, is_dark);
                }
                refresh_session_list_ui(&handle, &store);
            }
        }
    });

    // 7.8 Send message with 16ms RAF-equivalent streaming batcher & zero-copy sandbox persistence
    app.global::<AppState>().on_send_message({
        let records = records.clone();
        let stop_signal = stop_signal.clone();
        let app_weak = app.as_weak();
        let omni = omni.clone();
        let store = session_store.clone();
        let gateway = control_gateway.clone();
        let rt_handle = rt.handle().clone();

        move |user_text| {
            let user_str = user_text.to_string().trim().to_string();
            if user_str.is_empty() {
                return;
            }

            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            let is_dark = handle.global::<Theme>().get_dark();

            // Resolve active session ID (or create a session if none exists)
            let active_sid = UuidV7::parse_str(
                handle.global::<AppState>().get_current_session_id().as_str(),
            )
            .unwrap_or_else(|_| {
                let meta = store
                    .create_session(None)
                    .expect("Failed to create session");
                handle
                    .global::<AppState>()
                    .set_current_session_id(SharedString::from(meta.id.to_string()));
                meta.id
            });

            // Immediately persist user message into the isolated session sandbox
            let user_msg_id = store
                .append_message(active_sid, MessageRole::User, &user_str, Vec::new())
                .map(|m| m.id)
                .unwrap_or_else(|_| UuidV7::new_now());

            refresh_session_list_ui(&handle, &store);

            handle.global::<AppState>().set_has_messages(true);
            handle.global::<AppState>().set_is_generating(true);
            stop_signal.store(false, Ordering::SeqCst);

            // Clear stale selection on new message turn (Fix X6)
            UI_VIEWPORT_STATE.with(|cell| {
                let mut vp = cell.borrow_mut();
                vp.selected_indices.clear();
                vp.selection_anchor = None;
            });

            // Fix F6 & F7: single `accumulated_content` field + build prompt payloads under read lock without cloning records
            let prompt_messages = {
                let mut recs = records.write();
                recs.push(MessageRecord {
                    message_id: user_msg_id,
                    is_user: true,
                    accumulated_content: user_str,
                    is_streaming: false,
                    persisted: true,
                    automaton: Arc::new(RwLock::new(StreamBlockAutomaton::new())),
                });

                recs.push(MessageRecord {
                    message_id: UuidV7::new_now(),
                    is_user: false,
                    accumulated_content: String::new(),
                    is_streaming: true,
                    persisted: false,
                    automaton: Arc::new(RwLock::new(StreamBlockAutomaton::new())),
                });

                sync_and_cull_ui(&handle, &recs, is_dark);
                handle.invoke_scroll_to_bottom_if_sticky();

                recs.iter()
                    .filter(|r| !r.accumulated_content.is_empty())
                    .map(|r| ChatMessagePayload {
                        role: if r.is_user {
                            "user".to_string()
                        } else {
                            "assistant".to_string()
                        },
                        content: r.accumulated_content.clone(),
                    })
                    .collect::<Vec<_>>()
            };

            let records_clone = records.clone();
            let app_weak_clone = app_weak.clone();
            let stop_signal_clone = stop_signal.clone();
            let omni_clone = omni.clone();
            let store_clone = store.clone();
            let gateway_clone = gateway.clone();
            let turn_session_id = active_sid;

            rt_handle.spawn(async move {
                let config = match DeepSeekConfig::load_from_env() {
                    Ok(c) => c,
                    Err(e) => {
                        let store_err = store_clone.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(h) = app_weak_clone.upgrade() {
                                h.global::<AppState>().set_is_generating(false);
                                {
                                    let mut recs = records_clone.write();
                                    if let Some(last) = recs.last_mut() {
                                        last.accumulated_content = format!("⚠️ 配置错误：{}", e);
                                        last.is_streaming = false;
                                        let _ = persist_assistant_record_if_needed(
                                            &store_err,
                                            turn_session_id,
                                            last,
                                        );
                                    }
                                    let is_dark = h.global::<Theme>().get_dark();
                                    sync_and_cull_ui(&h, &recs, is_dark);
                                }
                                refresh_session_list_ui(&h, &store_err);
                                h.invoke_scroll_to_bottom_if_sticky();
                            }
                        });
                        return;
                    }
                };

                let client = DeepSeekClient::new();
                let (tx, mut rx) = tokio::sync::mpsc::channel(128);

                let stop_sub = stop_signal_clone.clone();
                tokio::spawn(async move {
                    client
                        .stream_chat(&config, prompt_messages, stop_sub, tx)
                        .await;
                });

                // 16ms frame-throttled streaming batcher (`session-update-batcher` equivalent)
                let mut pending_delta = String::new();
                let mut last_flush = Instant::now();
                let frame_interval = Duration::from_millis(16);

                while let Some(event) = rx.recv().await {
                    match event {
                        StreamEvent::ContentDelta(delta) => {
                            pending_delta.push_str(&delta);
                            while let Ok(StreamEvent::ContentDelta(more)) = rx.try_recv() {
                                pending_delta.push_str(&more);
                            }

                            if last_flush.elapsed() >= frame_interval {
                                let chunk = std::mem::take(&mut pending_delta);
                                last_flush = Instant::now();
                                let recs_lock = records_clone.clone();
                                let weak_app = app_weak_clone.clone();

                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(h) = weak_app.upgrade() {
                                        let mut recs = recs_lock.write();
                                        if let Some(last) = recs.last_mut() {
                                            last.accumulated_content.push_str(&chunk);
                                        }
                                        let is_dark = h.global::<Theme>().get_dark();
                                        sync_and_cull_ui(&h, &recs, is_dark);
                                        h.invoke_scroll_to_bottom_if_sticky();
                                    }
                                });
                            }
                        }
                        StreamEvent::ToolCall {
                            name,
                            arguments_json,
                        } => {
                            let flushed_chunk = std::mem::take(&mut pending_delta);
                            let recs_lock = records_clone.clone();
                            let weak_app = app_weak_clone.clone();
                            let omni = omni_clone.clone();
                            let gw = gateway_clone.clone();

                            let (summary, detail) =
                                if ClientControlCommand::from_tool_call(&name, &arguments_json)
                                    .is_ok()
                                {
                                    match gw.execute_tool_call_json(&name, &arguments_json) {
                                        Ok(resp_json) => (
                                            format!("执行客户端控制接口: {}", name),
                                            format!(
                                                "【客户端预留接口执行成功】\n接口名称: {}\n入参:\n{}\n响应:\n{}",
                                                name, arguments_json, resp_json
                                            ),
                                        ),
                                        Err(err) => (
                                            format!("客户端控制接口异常: {}", name),
                                            format!("入参:\n{}\n错误: {}", arguments_json, err),
                                        ),
                                    }
                                } else {
                                    let task_id = format!(
                                        "task-{}",
                                        chrono::Utc::now().timestamp_millis()
                                    );
                                    omni.memory.add_task(task_id.clone());
                                    omni.memory.intent_store.write().insert(
                                        "latest_tool_call".to_string(),
                                        arguments_json.clone(),
                                    );

                                    (
                                        format!("派发新任务 (AXIOM[03] 沙箱拦截): {}", name),
                                        format!(
                                            "【沙箱安全拦截】\n任务 ID: {}\n工具名称: {}\n入参配置:\n{}\n状态: 任务已原子写入活体节点记忆，安全阻断真实外发执行。",
                                            task_id, name, arguments_json
                                        ),
                                    )
                                };

                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(h) = weak_app.upgrade() {
                                    let mut recs = recs_lock.write();
                                    if let Some(last) = recs.last_mut() {
                                        if !flushed_chunk.is_empty() {
                                            last.accumulated_content.push_str(&flushed_chunk);
                                        }
                                        last.automaton.write().add_action_capsule(summary, detail);
                                    }
                                    let is_dark = h.global::<Theme>().get_dark();
                                    sync_and_cull_ui(&h, &recs, is_dark);
                                    h.invoke_scroll_to_bottom_if_sticky();
                                }
                            });
                        }
                        StreamEvent::Done => {
                            let flushed_chunk = std::mem::take(&mut pending_delta);
                            let recs_lock = records_clone.clone();
                            let weak_app = app_weak_clone.clone();
                            let store_done = store_clone.clone();

                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(h) = weak_app.upgrade() {
                                    h.global::<AppState>().set_is_generating(false);
                                    {
                                        let mut recs = recs_lock.write();
                                        if let Some(last) = recs.last_mut() {
                                            if !flushed_chunk.is_empty() {
                                                last.accumulated_content.push_str(&flushed_chunk);
                                            }
                                            last.is_streaming = false;
                                            let _ = persist_assistant_record_if_needed(
                                                &store_done,
                                                turn_session_id,
                                                last,
                                            );
                                        }
                                        let is_dark = h.global::<Theme>().get_dark();
                                        sync_and_cull_ui(&h, &recs, is_dark);
                                    }
                                    refresh_session_list_ui(&h, &store_done);
                                    h.invoke_scroll_to_bottom_if_sticky();
                                }
                            });
                            break;
                        }
                        StreamEvent::Error(err_msg) => {
                            let flushed_chunk = std::mem::take(&mut pending_delta);
                            let recs_lock = records_clone.clone();
                            let weak_app = app_weak_clone.clone();
                            let store_err = store_clone.clone();

                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(h) = weak_app.upgrade() {
                                    h.global::<AppState>().set_is_generating(false);
                                    {
                                        let mut recs = recs_lock.write();
                                        if let Some(last) = recs.last_mut() {
                                            if !flushed_chunk.is_empty() {
                                                last.accumulated_content.push_str(&flushed_chunk);
                                            }
                                            last.accumulated_content
                                                .push_str(&format!("\n\n❌ 请求出错：{}", err_msg));
                                            last.is_streaming = false;
                                            let _ = persist_assistant_record_if_needed(
                                                &store_err,
                                                turn_session_id,
                                                last,
                                            );
                                        }
                                        let is_dark = h.global::<Theme>().get_dark();
                                        sync_and_cull_ui(&h, &recs, is_dark);
                                    }
                                    refresh_session_list_ui(&h, &store_err);
                                    h.invoke_scroll_to_bottom_if_sticky();
                                }
                            });
                            break;
                        }
                    }
                }
            });
        }
    });

    app.run()
}
