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
pub mod io_worker;
pub mod latex_renderer;
pub mod markdown_parser;
pub mod selection_engine;
pub mod smooth_scroll;
pub mod stream_automaton;
pub mod text_metrics;
pub mod viewport_culler;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

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
use io_worker::{AsyncIoHandle, IoCommand};
use parking_lot::{Mutex, RwLock};
use selection_engine::{
    compute_block_highlight_rects, expand_block_range, expand_word_at,
    extract_block_selected_text, hit_test_block, make_span, BlockSelectableGeometry,
    DragGestureState, SelectableLine, SelectableSpan, TextPos, TextSelection,
};
use slint::{Color, ComponentHandle, Model, ModelRc, SharedString, VecModel};
use smooth_scroll::LenisScrollManager;
use stream_automaton::{BlockKind, StreamBlock, StreamBlockAutomaton};
use text_metrics::{wrap_plain_text_lines, TextMetricStyle};
use viewport_culler::FenwickViewportIndex;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

slint::include_modules!();

const DEFAULT_INLINE_COLS: usize = 68;
const VIEWPORT_OVERSCAN_PX: f32 = 640.0;

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
    viewport_width: f32,
    max_cols: usize,
    dark_mode: bool,
    text_selection: Option<TextSelection>,
    drag_gesture: DragGestureState,
    context_target_flat: Option<usize>,
    context_target_msg: Option<usize>,
    /// Persistent VecModel bound once to `MainWindow::visible-blocks` to enable incremental diffing.
    visible_model: Rc<VecModel<RenderBlockData>>,
    visible_model_bound: bool,
    mounted_range: Range<usize>,
    mounted_style: (bool, usize),
    /// Per-code-block horizontal scroll offset (`flat_idx -> scroll_x_px`).
    code_scroll_x: HashMap<usize, f32>,
    /// Per-code-block manual word-wrap toggle (`flat_idx -> wrap_enabled`).
    code_wrap_states: HashMap<usize, bool>,
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
            viewport_width: 800.0,
            max_cols: DEFAULT_INLINE_COLS,
            dark_mode: false,
            text_selection: None,
            drag_gesture: DragGestureState::default(),
            context_target_flat: None,
            context_target_msg: None,
            visible_model: Rc::new(VecModel::default()),
            visible_model_bound: false,
            mounted_range: 0..0,
            mounted_style: (false, DEFAULT_INLINE_COLS),
            code_scroll_x: HashMap::new(),
            code_wrap_states: HashMap::new(),
        }
    }
}

thread_local! {
    static UI_VIEWPORT_STATE: RefCell<UiViewportState> = RefCell::new(UiViewportState::default());
    static LENIS_STATE: RefCell<LenisScrollManager> = const { RefCell::new(LenisScrollManager::new()) };
    static LAST_SENT_CHAT_Y: Cell<f32> = const { Cell::new(f32::NAN) };
}

/// Advances all active Lenis scroll axes by one VSync-locked frame tick and deactivates the
/// Slint `animation-tick()` clock on sub-pixel convergence so idle CPU usage is strictly 0%.
fn step_lenis_frame(handle: &MainWindow) {
    let (chat_y, sidebar_y, input_y, _code_x, still_active) = LENIS_STATE.with(|cell| {
        let mut mgr = cell.borrow_mut();
        let (c, s, i, h) = mgr.step_all();
        (c, s, i, h, mgr.any_active())
    });

    if let Some(y) = chat_y {
        let emit_y = if std::env::var_os("FUGUE_HW_RENDER").is_some() {
            y
        } else {
            y.round()
        };
        let should_emit = LAST_SENT_CHAT_Y.with(|last| {
            let prev = last.get();
            if !still_active || prev.is_nan() || (emit_y - prev).abs() >= 0.001 {
                last.set(emit_y);
                true
            } else {
                false
            }
        });
        if should_emit {
            handle.invoke_set_chat_scroll_y(emit_y);
            UI_VIEWPORT_STATE.with(|cell| {
                let mut vp = cell.borrow_mut();
                let old_slice = vp.fenwick.query_viewport(
                    vp.scroll_y,
                    vp.viewport_height,
                    VIEWPORT_OVERSCAN_PX,
                );
                vp.scroll_y = y.max(0.0);
                let new_slice = vp.fenwick.query_viewport(
                    vp.scroll_y,
                    vp.viewport_height,
                    VIEWPORT_OVERSCAN_PX,
                );
                if old_slice.start_idx != new_slice.start_idx
                    || old_slice.end_idx != new_slice.end_idx
                {
                    apply_viewport_slice(handle, &mut vp);
                }
            });
        }
    }
    if let Some(y) = sidebar_y {
        handle.invoke_set_sidebar_scroll_y(y);
    }
    if let Some(y) = input_y {
        handle.invoke_set_input_scroll_y(y);
    }

    if !still_active {
        LAST_SENT_CHAT_Y.with(|last| last.set(f32::NAN));
        let state = handle.global::<AppState>();
        if state.get_lenis_active() {
            state.set_lenis_active(false);
        }
    }
}

/// Activates the VSync-synchronized Slint `animation-tick()` Lenis loop on demand.
fn ensure_lenis_timer_running(app_weak: slint::Weak<MainWindow>) {
    let Some(handle) = app_weak.upgrade() else {
        return;
    };
    let state = handle.global::<AppState>();
    if !state.get_lenis_active() {
        state.set_lenis_active(true);
    }
    handle.window().request_redraw();
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

fn apply_search_hits_to_ui(window: &MainWindow, hits: Vec<fugue_core::SearchMatchHit>) {
    let is_zh = window.global::<AppState>().get_lang() == "zh";
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

fn refresh_search_hits_ui(window: &MainWindow, store: &SessionSandboxStore, query: &str) {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        window
            .global::<AppState>()
            .set_search_hits(ModelRc::default());
        return;
    }
    let hits = store.search_content(None, trimmed, 60);
    apply_search_hits_to_ui(window, hits);
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

struct GenerationCoordinator {
    current_gen_id: AtomicU64,
    active_cancel: Mutex<Option<tokio::sync::watch::Sender<bool>>>,
    active_tasks: Mutex<Vec<tokio::task::AbortHandle>>,
}

impl GenerationCoordinator {
    fn new() -> Self {
        Self {
            current_gen_id: AtomicU64::new(0),
            active_cancel: Mutex::new(None),
            active_tasks: Mutex::new(Vec::new()),
        }
    }

    fn begin_generation(&self) -> (u64, tokio::sync::watch::Receiver<bool>) {
        self.cancel_active();
        let gen_id = self.current_gen_id.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = tokio::sync::watch::channel(false);
        *self.active_cancel.lock() = Some(tx);
        (gen_id, rx)
    }

    fn register_task(&self, gen_id: u64, handle: tokio::task::AbortHandle) {
        if self.is_current(gen_id) {
            self.active_tasks.lock().push(handle);
        } else {
            handle.abort();
        }
    }

    fn cancel_active(&self) {
        self.current_gen_id.fetch_add(1, Ordering::SeqCst);
        if let Some(tx) = self.active_cancel.lock().take() {
            let _ = tx.send(true);
        }
        let tasks = std::mem::take(&mut *self.active_tasks.lock());
        for handle in tasks {
            handle.abort();
        }
    }

    fn is_current(&self, gen_id: u64) -> bool {
        self.current_gen_id.load(Ordering::SeqCst) == gen_id
    }
}

fn finalize_active_generation(
    window: &MainWindow,
    store: &SessionSandboxStore,
    records: &Arc<RwLock<Vec<MessageRecord>>>,
    gen_coord: &GenerationCoordinator,
) {
    gen_coord.cancel_active();
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
        vp.text_selection = None;
        vp.drag_gesture.release();
        vp.context_target_flat = None;
        vp.context_target_msg = None;
        vp.visible_model.set_vec(Vec::new());
        vp.mounted_range = 0..0;
        vp.code_scroll_x.clear();
        vp.code_wrap_states.clear();
    });

    LAST_SENT_CHAT_Y.with(|last| last.set(f32::NAN));
    LENIS_STATE.with(|cell| {
        let mut mgr = cell.borrow_mut();
        mgr.chat.jump_to(0.0, 0.0);
        mgr.sidebar.jump_to(0.0, 0.0);
        mgr.input.jump_to(0.0, 0.0);
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
    state.set_is_drag_selecting(false);
    state.set_context_menu_open(false);
    state.set_session_menu_open(false);
    state.set_renaming_session_id(SharedString::default());
    window.invoke_clear_input_text();

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

fn load_safe_local_image(url: &str) -> slint::Image {
    let trimmed = url.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("data:")
    {
        return slint::Image::default();
    }
    let path = std::path::Path::new(trimmed);
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    if !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp" | "bmp") {
        return slint::Image::default();
    }
    let Ok(canon) = path.canonicalize() else {
        return slint::Image::default();
    };
    let in_cwd = std::env::current_dir()
        .ok()
        .and_then(|d| d.canonicalize().ok())
        .map(|d| canon.starts_with(&d))
        .unwrap_or(false);
    let in_fugue = fugue_kernel::resolve_user_fugue_root()
        .canonicalize()
        .ok()
        .map(|d| canon.starts_with(&d))
        .unwrap_or(false);
    if in_cwd || in_fugue {
        slint::Image::load_from_path(&canon).unwrap_or_default()
    } else {
        slint::Image::default()
    }
}

fn is_safe_http_url(url: &str) -> bool {
    let u = url.trim();
    if !(u.starts_with("http://") || u.starts_with("https://")) {
        return false;
    }
    !u.chars().any(|c| c.is_control() || c.is_whitespace())
}

fn open_external_url_safe(url: &str) -> bool {
    let u = url.trim();
    if !is_safe_http_url(u) {
        return false;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", u])
            .spawn()
            .is_ok()
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

fn toggle_task_checkbox_in_markdown(block_md: &str, item_idx: usize) -> Option<String> {
    let mut lines: Vec<String> = block_md.lines().map(|l| l.to_string()).collect();
    let mut list_item_counter = 0usize;
    let mut changed = false;
    for line in &mut lines {
        let trimmed = line.trim_start();
        let indent_len = line.len() - trimmed.len();
        let rest_after_bullet = if let Some(r) = trimmed.strip_prefix("- ") {
            Some((indent_len + 2, r))
        } else if let Some(r) = trimmed.strip_prefix("* ") {
            Some((indent_len + 2, r))
        } else if let Some(dot_pos) = trimmed.find(". ") {
            let prefix = &trimmed[..dot_pos];
            if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit()) {
                Some((indent_len + dot_pos + 2, &trimmed[dot_pos + 2..]))
            } else {
                None
            }
        } else {
            None
        };
        if let Some((prefix_end, rest)) = rest_after_bullet {
            if list_item_counter == item_idx {
                if let Some(after_box) = rest.strip_prefix("[ ] ") {
                    *line = format!("{}[x] {}", &line[..prefix_end], after_box);
                    changed = true;
                } else if let Some(after_box) = rest
                    .strip_prefix("[x] ")
                    .or_else(|| rest.strip_prefix("[X] "))
                {
                    *line = format!("{}[ ] {}", &line[..prefix_end], after_box);
                    changed = true;
                }
                break;
            }
            list_item_counter += 1;
        }
    }
    if changed {
        Some(lines.join("\n"))
    } else {
        None
    }
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
                    let image_source = if sp.text.starts_with("[🖼 ") && !sp.link_url.is_empty() {
                        load_safe_local_image(&sp.link_url)
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
                        image_source,
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
        selection_rects: ModelRc::default(),
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
    viewport_width: f32,
    selected: bool,
    measured_heights: &HashMap<(usize, String), f32>,
) -> (FlatBlockEntry, f32) {
    let block_id = format!("user-{}", msg_idx);
    let wrap_w = (viewport_width * 0.72).clamp(180.0, 492.0);
    let wrapped_lines = wrap_plain_text_lines(raw_text, wrap_w, 14.0);
    let line_cnt = wrapped_lines.len().max(1);
    let rich_models: Vec<RichLineData> = wrapped_lines
        .into_iter()
        .map(|line_text| RichLineData {
            spans: ModelRc::new(VecModel::from(vec![InlineSpanData {
                text: SharedString::from(line_text),
                bold: false,
                italic: false,
                code: false,
                strikethrough: false,
                math: false,
                math_image: slint::Image::default(),
                image_source: slint::Image::default(),
                link_url: SharedString::default(),
            }])),
        })
        .collect();
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
        rich_lines: ModelRc::new(VecModel::from(rich_models)),
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
        selection_rects: ModelRc::default(),
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
                    clamp_selection_to_len(&mut vp_state.text_selection, total_len);
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
                let selected = is_block_touched_by_selection(vp_state.text_selection, flat_idx);
                let (entry, est_h) = make_user_block_entry(
                    flat_idx,
                    msg_idx,
                    &rec.accumulated_content,
                    vp_state.viewport_width,
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
        clamp_selection_to_len(&mut vp_state.text_selection, total_len);
        apply_viewport_slice(window, &mut vp_state);
    });
}

#[inline]
fn clamp_selection_to_len(sel: &mut Option<TextSelection>, total_len: usize) {
    if total_len == 0 {
        *sel = None;
        return;
    }
    if let Some(ref mut s) = sel {
        let max_idx = total_len - 1;
        if s.anchor.flat_index > max_idx {
            s.anchor.flat_index = max_idx;
        }
        if s.focus.flat_index > max_idx {
            s.focus.flat_index = max_idx;
        }
    }
}

#[inline]
fn is_block_touched_by_selection(sel: Option<TextSelection>, flat_idx: usize) -> bool {
    match sel {
        Some(s) if !s.is_collapsed() => {
            let (start, end) = s.ordered();
            flat_idx >= start.flat_index && flat_idx <= end.flat_index
        }
        _ => false,
    }
}

/// Compiles or retrieves a cached `RenderBlockData` for `flat_blocks[idx]`.
fn compile_entry_render_data(idx: usize, vp_state: &mut UiViewportState) -> Option<RenderBlockData> {
    let dark_mode = vp_state.dark_mode;
    let max_cols = vp_state.max_cols;
    let viewport_width = vp_state.viewport_width;
    let text_sel = vp_state.text_selection;
    let selected = is_block_touched_by_selection(text_sel, idx);
    let block_h = vp_state.fenwick.get_height(idx);
    let code_sx = vp_state.code_scroll_x.get(&idx).copied().unwrap_or(0.0);
    let code_wrap = vp_state.code_wrap_states.get(&idx).copied().unwrap_or(false);
    let entry = &vp_state.flat_blocks[idx];

    if entry.is_user {
        let mut d = entry.render_data.as_ref()?.clone();
        d.flat_index = idx as i32;
        d.selected = selected;
        d.selection_rects = build_selection_rects_model(
            entry,
            &d,
            viewport_width,
            block_h,
            code_sx,
            code_wrap,
            text_sel,
        );
        return Some(d);
    }

    let msg_idx = entry.msg_index;
    let b_idx = entry.block_index;
    let is_first = entry.is_first_in_msg;
    let is_tail = entry.is_active_tail;
    let block_id = entry.block_id.clone();

    let compiled_tail = if is_tail {
        let sb = entry.source_block.clone()?;
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
        Some(rdata)
    } else {
        let cache_key = (msg_idx, block_id);
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
                if vp_state.frozen_cache.len() >= 512 {
                    let mounted_msgs: std::collections::HashSet<usize> = vp_state
                        .flat_blocks
                        .get(vp_state.mounted_range.clone())
                        .map(|slice| slice.iter().map(|b| b.msg_index).collect())
                        .unwrap_or_default();
                    let evict_keys: Vec<(usize, String)> = vp_state
                        .frozen_cache
                        .keys()
                        .filter(|(m, _)| !mounted_msgs.contains(m))
                        .take(128)
                        .cloned()
                        .collect();
                    for k in evict_keys {
                        vp_state.frozen_cache.remove(&k);
                    }
                }
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

        vp_state.frozen_cache.get(&cache_key).map(|cached| {
            let mut d = cached.render_data.clone();
            d.flat_index = idx as i32;
            d.msg_index = msg_idx as i32;
            d.block_index = b_idx as i32;
            d.selected = selected;
            d.is_first_in_msg = is_first;
            d
        })
    };

    compiled_tail.map(|mut d| {
        let entry_ref = &vp_state.flat_blocks[idx];
        d.selection_rects = build_selection_rects_model(
            entry_ref,
            &d,
            viewport_width,
            block_h,
            code_sx,
            code_wrap,
            text_sel,
        );
        d
    })
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

    if !vp_state.visible_model_bound {
        window.set_visible_blocks(ModelRc::from(vp_state.visible_model.clone()));
        vp_state.visible_model_bound = true;
    }

    let new_range = slice.start_idx..slice.end_idx;
    let old_range = vp_state.mounted_range.clone();
    let style_changed = vp_state.mounted_style != (dark_mode, max_cols);
    let current_row_count = vp_state.visible_model.row_count();

    if new_range.is_empty() {
        if current_row_count > 0 {
            vp_state.visible_model.set_vec(Vec::new());
        }
    } else if style_changed
        || old_range.is_empty()
        || current_row_count != old_range.len()
        || new_range.end <= old_range.start
        || new_range.start >= old_range.end
    {
        let mut visible = Vec::with_capacity(new_range.len());
        for idx in new_range.clone() {
            if let Some(d) = compile_entry_render_data(idx, vp_state) {
                visible.push(d);
            }
        }
        vp_state.visible_model.set_vec(visible);
    } else {
        let overlap_start = old_range.start.max(new_range.start);
        let overlap_end = old_range.end.min(new_range.end);

        // 1. Trim items scrolled off the top
        for _ in old_range.start..overlap_start {
            vp_state.visible_model.remove(0);
        }
        // 2. Trim items scrolled off the bottom
        for _ in overlap_end..old_range.end {
            let last_idx = vp_state.visible_model.row_count().saturating_sub(1);
            vp_state.visible_model.remove(last_idx);
        }
        // 3. Patch only changed items within the surviving overlap range (zero clone for static blocks)
        for idx in overlap_start..overlap_end {
            let local_idx = idx - overlap_start;
            let selected = is_block_touched_by_selection(vp_state.text_selection, idx);
            let entry = &vp_state.flat_blocks[idx];
            let wanted_expanded = match entry.source_block.as_ref().map(|s| &s.kind) {
                Some(BlockKind::ActionCapsule { is_expanded, .. }) => *is_expanded,
                _ => false,
            };
            let needs_patch = match vp_state.visible_model.row_data(local_idx) {
                Some(old_item) => {
                    entry.is_active_tail
                        || old_item.flat_index != idx as i32
                        || old_item.msg_index != entry.msg_index as i32
                        || old_item.block_index != entry.block_index as i32
                        || old_item.selected != selected
                        || old_item.is_expanded != wanted_expanded
                        || old_item.is_first_in_msg != entry.is_first_in_msg
                }
                None => true,
            };
            if needs_patch {
                if let Some(new_item) = compile_entry_render_data(idx, vp_state) {
                    vp_state.visible_model.set_row_data(local_idx, new_item);
                }
            }
        }
        // 4. Prepend items newly entering at the top
        for idx in (new_range.start..overlap_start).rev() {
            if let Some(new_item) = compile_entry_render_data(idx, vp_state) {
                vp_state.visible_model.insert(0, new_item);
            }
        }
        // 5. Append items newly entering at the bottom
        for idx in overlap_end..new_range.end {
            if let Some(new_item) = compile_entry_render_data(idx, vp_state) {
                vp_state.visible_model.push(new_item);
            }
        }
    }

    vp_state.mounted_range = new_range;
    vp_state.mounted_style = (dark_mode, max_cols);

    // Re-read spacer heights in case lazy compilation updated an unmeasured block height
    let updated_slice = vp_state.fenwick.query_viewport(
        vp_state.scroll_y,
        vp_state.viewport_height,
        VIEWPORT_OVERSCAN_PX,
    );

    let state = window.global::<AppState>();
    state.set_top_spacer_height(updated_slice.top_spacer_height);
    state.set_bottom_spacer_height(updated_slice.bottom_spacer_height);
    let has_sel = vp_state
        .text_selection
        .map(|s| !s.is_collapsed())
        .unwrap_or(false);
    state.set_selected_block_count(if has_sel { 1 } else { 0 });
}

fn build_rich_line_selectable(
    line: RichLineData,
    x0: f32,
    y_top: f32,
    font_size: f32,
    default_bold: bool,
    line_height: f32,
) -> SelectableLine {
    let mut spans = Vec::new();
    let mut cur_x = x0;
    let mut char_start = 0usize;
    let mut plain = String::new();
    let mut max_line_h = line_height;

    for span in line.spans.iter() {
        let text = span.text.to_string();
        let cc = text.chars().count();
        let img_sz = span.math_image.size();
        if span.math && img_sz.width > 0 {
            let img_w = img_sz.width as f32;
            let img_h = img_sz.height as f32 + 4.0;
            max_line_h = max_line_h.max(img_h);
            let mut offsets = Vec::with_capacity(cc + 1);
            offsets.push(0.0);
            for i in 1..=cc {
                offsets.push(img_w * (i as f32) / (cc.max(1) as f32));
            }
            spans.push(SelectableSpan {
                text: text.clone(),
                style: TextMetricStyle::MathSerif,
                font_size,
                x_offset: cur_x + 2.0,
                width: img_w,
                char_start,
                char_offsets: offsets,
            });
            cur_x += img_w + 4.0;
            char_start += cc;
            plain.push_str(&text);
            continue;
        }

        let is_bold = span.bold || default_bold;
        let style = if span.code {
            TextMetricStyle::Mono
        } else if span.math {
            TextMetricStyle::MathSerif
        } else if is_bold && span.italic {
            TextMetricStyle::BoldItalic
        } else if is_bold {
            TextMetricStyle::Bold
        } else if span.italic {
            TextMetricStyle::Italic
        } else {
            TextMetricStyle::Regular
        };
        let eff_size = if span.code { font_size - 1.0 } else { font_size };
        let lead_pad = if span.code { 5.0 } else { 0.0 };
        let trail_pad = if span.code { 5.0 } else { 0.0 };
        let s = make_span(
            text.clone(),
            style,
            eff_size,
            cur_x + lead_pad,
            char_start,
            0.0,
        );
        cur_x += s.width + lead_pad + trail_pad;
        char_start += cc;
        plain.push_str(&text);
        spans.push(s);
    }

    SelectableLine {
        y_top,
        height: max_line_h,
        spans,
        plain_text: plain,
        char_count: char_start,
        is_table_row: false,
        table_cell_ranges: Vec::new(),
    }
}

fn build_block_selectable_geometry(
    entry: &FlatBlockEntry,
    rdata: &RenderBlockData,
    viewport_width: f32,
    block_height_px: f32,
    code_scroll_x: f32,
    code_wrap: bool,
) -> BlockSelectableGeometry {
    let top_pad = if rdata.is_first_in_msg { 8.0 } else { 2.0 };
    let mut lines = Vec::new();

    match rdata.block_type {
        BlockType::UserBubble => {
            let mut max_line_w = 0.0f32;
            let mut temp_lines = Vec::new();
            for rl in rdata.rich_lines.iter() {
                let sl = build_rich_line_selectable(rl, 0.0, 0.0, 14.0, false, 19.0);
                let w: f32 = sl
                    .spans
                    .last()
                    .map(|s| s.x_offset + s.width)
                    .unwrap_or(0.0);
                if w > max_line_w {
                    max_line_w = w;
                }
                temp_lines.push(sl);
            }
            let bubble_w = (max_line_w + 28.0).clamp(48.0, 520.0);
            let bubble_x = (viewport_width - 6.0 - bubble_w).max(6.0);
            let text_x = bubble_x + 14.0;
            let mut y = top_pad + 12.0;
            for mut sl in temp_lines {
                sl.y_top = y;
                for sp in &mut sl.spans {
                    sp.x_offset += text_x;
                }
                y += sl.height + 4.0;
                lines.push(sl);
            }
        }
        BlockType::Paragraph | BlockType::ActiveTail => {
            let mut y = top_pad;
            for rl in rdata.rich_lines.iter() {
                let sl = build_rich_line_selectable(rl, 6.0, y, 14.0, false, 19.0);
                y += sl.height + 4.0;
                lines.push(sl);
            }
        }
        BlockType::Heading1 => {
            let mut y = top_pad;
            for rl in rdata.rich_lines.iter() {
                let sl = build_rich_line_selectable(rl, 6.0, y, 20.0, true, 26.0);
                y += sl.height + 4.0;
                lines.push(sl);
            }
        }
        BlockType::Heading2 => {
            let mut y = top_pad;
            for rl in rdata.rich_lines.iter() {
                let sl = build_rich_line_selectable(rl, 6.0, y, 17.0, true, 23.0);
                y += sl.height + 4.0;
                lines.push(sl);
            }
        }
        BlockType::Heading3 => {
            let mut y = top_pad;
            for rl in rdata.rich_lines.iter() {
                let sl = build_rich_line_selectable(rl, 6.0, y, 15.0, true, 21.0);
                y += sl.height + 4.0;
                lines.push(sl);
            }
        }
        BlockType::Blockquote => {
            let mut y = top_pad + 10.0;
            for rl in rdata.rich_lines.iter() {
                let sl = build_rich_line_selectable(rl, 23.5, y, 14.0, false, 19.0);
                y += sl.height + 4.0;
                lines.push(sl);
            }
        }
        BlockType::ListBlock => {
            let mut y = top_pad;
            for item in rdata.list_items.iter() {
                let x0 = 6.0 + (item.indent_level.max(0) as f32) * 18.0 + 20.0 + 8.0;
                y += 1.0;
                for rl in item.lines.iter() {
                    let sl = build_rich_line_selectable(rl, x0, y, 14.0, false, 19.0);
                    y += sl.height + 4.0;
                    lines.push(sl);
                }
                y += 2.0; // remaining item spacing (6px total with 4px paragraph spacing)
            }
        }
        BlockType::CodeBlock | BlockType::ActiveCodeTail => {
            let mut y = top_pad + 32.0 + 12.0;
            let use_wrapped = rdata.block_type == BlockType::ActiveCodeTail || code_wrap;
            if !use_wrapped && rdata.code_lines.row_count() > 0 {
                let x0 = 6.0 + 12.0 + 44.0 - code_scroll_x;
                for cl in rdata.code_lines.iter() {
                    let mut spans = Vec::new();
                    let mut cur_x = x0;
                    let mut char_start = 0usize;
                    let mut plain = String::new();
                    for tok in cl.tokens.iter() {
                        let t = tok.text.to_string();
                        let s = make_span(t.clone(), TextMetricStyle::Mono, 13.0, cur_x, char_start, 0.0);
                        cur_x += s.width;
                        char_start += t.chars().count();
                        plain.push_str(&t);
                        spans.push(s);
                    }
                    lines.push(SelectableLine {
                        y_top: y,
                        height: 17.5,
                        spans,
                        plain_text: plain,
                        char_count: char_start,
                        is_table_row: false,
                        table_cell_ranges: Vec::new(),
                    });
                    y += 19.5;
                }
            } else {
                let x0 = 6.0 + 12.0;
                let wrap_w = (viewport_width - 36.0).max(120.0);
                for raw_l in wrap_plain_text_lines(rdata.text.as_str(), wrap_w, 13.0) {
                    let s = make_span(raw_l.clone(), TextMetricStyle::Mono, 13.0, x0, 0, 0.0);
                    let cc = raw_l.chars().count();
                    lines.push(SelectableLine {
                        y_top: y,
                        height: 17.5,
                        spans: vec![s],
                        plain_text: raw_l,
                        char_count: cc,
                        is_table_row: false,
                        table_cell_ranges: Vec::new(),
                    });
                    y += 17.5;
                }
            }
        }
        BlockType::TableBlock => {
            let col_count = rdata.table_headers.row_count().max(1);
            let avail_w = (viewport_width - 12.0).max(120.0);
            let inner_w = (avail_w - 24.0 - ((col_count.saturating_sub(1)) as f32) * 12.0).max(60.0);
            let col_w = inner_w / (col_count as f32);
            let aligns: Vec<i32> = rdata.table_aligns.iter().collect();
            let mut y = top_pad + 29.0;

            let build_table_row = |cells: Vec<String>, row_y: f32, min_row_h: f32, bold: bool| -> (SelectableLine, f32) {
                let mut spans = Vec::new();
                let mut ranges = Vec::new();
                let mut char_start = 0usize;
                let mut plain = String::new();
                let mut max_lines = 1usize;
                let style = if bold { TextMetricStyle::Bold } else { TextMetricStyle::Regular };
                for (c_idx, cell) in cells.into_iter().enumerate() {
                    let wrapped = wrap_plain_text_lines(&cell, col_w.max(24.0), 13.0);
                    max_lines = max_lines.max(wrapped.len().max(1));
                    let first_line = wrapped.first().map(|s| s.as_str()).unwrap_or("");
                    let s_measure = make_span(first_line.to_string(), style, 13.0, 0.0, 0, 0.0);
                    let col_x0 = 6.0 + 12.0 + (c_idx as f32) * (col_w + 12.0);
                    let align = aligns.get(c_idx).copied().unwrap_or(0);
                    let cell_x = match align {
                        1 => col_x0 + ((col_w - s_measure.width).max(0.0) / 2.0),
                        2 => col_x0 + (col_w - s_measure.width).max(0.0),
                        _ => col_x0,
                    };
                    let cc = cell.chars().count();
                    let s = make_span(cell.clone(), style, 13.0, cell_x, char_start, 0.0);
                    spans.push(s);
                    ranges.push((char_start, char_start + cc));
                    char_start += cc;
                    plain.push_str(&cell);
                }
                let actual_h = min_row_h.max((max_lines as f32) * 17.0 + 12.0);
                (
                    SelectableLine {
                        y_top: row_y,
                        height: actual_h,
                        spans,
                        plain_text: plain,
                        char_count: char_start,
                        is_table_row: true,
                        table_cell_ranges: ranges,
                    },
                    actual_h,
                )
            };

            let headers: Vec<String> = rdata.table_headers.iter().map(|s| s.to_string()).collect();
            if !headers.is_empty() {
                let (line, h) = build_table_row(headers, y, 34.0, true);
                lines.push(line);
                y += h;
            }
            for row in rdata.table_rows.iter() {
                let cells: Vec<String> = row.cells.iter().map(|s| s.to_string()).collect();
                let (line, h) = build_table_row(cells, y, 32.0, false);
                lines.push(line);
                y += h;
            }
        }
        BlockType::LatexMath | BlockType::DiagramBlock | BlockType::ActionCapsule | BlockType::ThematicBreak => {
            let h = (block_height_px - top_pad - 10.0).max(20.0);
            return BlockSelectableGeometry {
                lines: Vec::new(),
                is_atomic: true,
                atomic_rect: (6.0, top_pad, (viewport_width - 12.0).max(40.0), h),
            };
        }
    }

    let _ = entry;
    BlockSelectableGeometry {
        lines,
        is_atomic: false,
        atomic_rect: (0.0, 0.0, 0.0, 0.0),
    }
}

fn build_selection_rects_model(
    entry: &FlatBlockEntry,
    rdata: &RenderBlockData,
    viewport_width: f32,
    block_height_px: f32,
    code_scroll_x: f32,
    code_wrap: bool,
    text_sel: Option<TextSelection>,
) -> ModelRc<SelectionRectData> {
    let Some(sel) = text_sel else {
        return ModelRc::default();
    };
    if sel.is_collapsed() || !is_block_touched_by_selection(Some(sel), entry.flat_index) {
        return ModelRc::default();
    }
    let geom = build_block_selectable_geometry(
        entry,
        rdata,
        viewport_width,
        block_height_px,
        code_scroll_x,
        code_wrap,
    );
    let rects = compute_block_highlight_rects(entry.flat_index, &geom, sel);
    if rects.is_empty() {
        return ModelRc::default();
    }
    let slint_rects: Vec<SelectionRectData> = rects
        .into_iter()
        .map(|r| SelectionRectData {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        })
        .collect();
    ModelRc::new(VecModel::from(slint_rects))
}

fn refresh_selection_overlays(window: &MainWindow, vp_state: &mut UiViewportState) {
    let viewport_width = vp_state.viewport_width;
    let text_sel = vp_state.text_selection;
    let mounted = vp_state.mounted_range.clone();
    for (local_idx, flat_idx) in mounted.enumerate() {
        if let Some(mut row) = vp_state.visible_model.row_data(local_idx) {
            if let Some(entry) = vp_state.flat_blocks.get(flat_idx) {
                let touched = is_block_touched_by_selection(text_sel, flat_idx);
                let block_h = vp_state.fenwick.get_height(flat_idx);
                let code_sx = vp_state.code_scroll_x.get(&flat_idx).copied().unwrap_or(0.0);
                let code_wrap = vp_state.code_wrap_states.get(&flat_idx).copied().unwrap_or(false);
                let new_rects = build_selection_rects_model(
                    entry,
                    &row,
                    viewport_width,
                    block_h,
                    code_sx,
                    code_wrap,
                    text_sel,
                );
                if row.selected != touched
                    || row.selection_rects.row_count() > 0
                    || new_rects.row_count() > 0
                {
                    row.selected = touched;
                    row.selection_rects = new_rects;
                    vp_state.visible_model.set_row_data(local_idx, row);
                }
            }
        }
    }
    let has_sel = text_sel.map(|s| !s.is_collapsed()).unwrap_or(false);
    window
        .global::<AppState>()
        .set_selected_block_count(if has_sel { 1 } else { 0 });
}

fn get_or_compile_geometry(
    flat_idx: usize,
    vp_state: &mut UiViewportState,
) -> Option<BlockSelectableGeometry> {
    let rdata = compile_entry_render_data(flat_idx, vp_state)?;
    let block_h = vp_state.fenwick.get_height(flat_idx);
    let viewport_w = vp_state.viewport_width;
    let code_sx = vp_state.code_scroll_x.get(&flat_idx).copied().unwrap_or(0.0);
    let code_wrap = vp_state.code_wrap_states.get(&flat_idx).copied().unwrap_or(false);
    let entry = vp_state.flat_blocks.get(flat_idx)?;
    Some(build_block_selectable_geometry(
        entry,
        &rdata,
        viewport_w,
        block_h,
        code_sx,
        code_wrap,
    ))
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

fn serialize_blocks_for_clipboard(vp_state: &mut UiViewportState, mode: &str) -> String {
    if let Some(sel) = vp_state.text_selection {
        if !sel.is_collapsed() {
            let (start, end) = sel.ordered();
            let mut chunks = Vec::new();
            for idx in start.flat_index..=end.flat_index {
                let Some(entry) = vp_state.flat_blocks.get(idx).cloned() else {
                    continue;
                };
                if mode == "copy_markdown" {
                    let is_full_middle = idx > start.flat_index && idx < end.flat_index;
                    if is_full_middle {
                        let chunk = serialize_single_block(&entry, mode);
                        if !chunk.is_empty() {
                            chunks.push(chunk);
                        }
                        continue;
                    }
                    if let Some(geom) = get_or_compile_geometry(idx, vp_state) {
                        let (full_s, full_e) = expand_block_range(idx, &geom);
                        let covers_all = (idx > start.flat_index || start <= full_s)
                            && (idx < end.flat_index || end >= full_e);
                        if covers_all {
                            let chunk = serialize_single_block(&entry, mode);
                            if !chunk.is_empty() {
                                chunks.push(chunk);
                            }
                        } else {
                            let part = extract_block_selected_text(idx, &geom, &entry.raw_markdown, sel);
                            if !part.is_empty() {
                                chunks.push(part);
                            }
                        }
                    }
                } else if let Some(geom) = get_or_compile_geometry(idx, vp_state) {
                    let part = extract_block_selected_text(idx, &geom, &entry.raw_markdown, sel);
                    if !part.is_empty() {
                        chunks.push(part);
                    }
                }
            }
            return chunks.join("\n\n");
        }
    }

    if let Some(idx) = vp_state.context_target_flat {
        if let Some(entry) = vp_state.flat_blocks.get(idx) {
            return serialize_single_block(entry, mode);
        }
    }
    String::new()
}

fn main() -> Result<(), slint::PlatformError> {
    if std::env::var_os("SLINT_BACKEND").is_none()
        && std::env::var_os("FUGUE_HW_RENDER").is_none()
    {
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

    // 4. Thread-safe message state & generation coordinator
    let records: Arc<RwLock<Vec<MessageRecord>>> = Arc::new(RwLock::new(Vec::new()));
    let gen_coord = Arc::new(GenerationCoordinator::new());

    // 5. Initialize Reserved LLM Client Control Gateway (with UI synchronization hook)
    let gateway_ui_hook = {
        let app_weak = app.as_weak();
        let store_for_hook = session_store.clone();
        let records_for_hook = records.clone();
        let gen_coord_for_hook = gen_coord.clone();
        Arc::new(move |resp: &ClientControlResponse| {
            let resp_clone = resp.clone();
            let weak = app_weak.clone();
            let store = store_for_hook.clone();
            let recs = records_for_hook.clone();
            let gen_coord = gen_coord_for_hook.clone();
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
                            finalize_active_generation(&handle, &store, &recs, &gen_coord);
                            let next_id = store
                                .list_sessions(false)
                                .first()
                                .map(|m| m.id)
                                .or_else(|| store.list_sessions(true).first().map(|m| m.id))
                                .or_else(|| store.create_session(None).ok().map(|m| m.id));
                            if let Some(nid) = next_id {
                                load_session_into_ui(&handle, &store, &recs, nid);
                            } else {
                                eprintln!("[fugue-ui] Failed to create fallback session after deletion");
                                refresh_session_list_ui(&handle, &store);
                            }
                        } else {
                            refresh_session_list_ui(&handle, &store);
                        }
                    }
                    ClientControlResponse::SessionCreatedOrUpdated { session } => {
                        let cur_sid = handle.global::<AppState>().get_current_session_id();
                        if session.archived && cur_sid.as_str() == session.id.to_string().as_str() {
                            finalize_active_generation(&handle, &store, &recs, &gen_coord);
                            let next_id = store
                                .list_sessions(false)
                                .first()
                                .map(|m| m.id)
                                .or_else(|| store.create_session(None).ok().map(|m| m.id));
                            if let Some(nid) = next_id {
                                load_session_into_ui(&handle, &store, &recs, nid);
                            } else {
                                refresh_session_list_ui(&handle, &store);
                            }
                        } else {
                            refresh_session_list_ui(&handle, &store);
                        }
                    }
                    ClientControlResponse::SearchResults { query, .. } => {
                        handle
                            .global::<AppState>()
                            .set_session_search_query(SharedString::from(&query));
                        refresh_search_hits_ui(&handle, &store, &query);
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

    // 6. Tokio background runtime & async I/O worker
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("Failed to initialize Tokio runtime");

    let io_latest_gen = Arc::new(AtomicU64::new(0));
    let io_handle = {
        let app_weak = app.as_weak();
        let latest_gen = io_latest_gen.clone();
        AsyncIoHandle::spawn(rt.handle(), session_store.clone(), move |gen_id, _q, hits| {
            let weak = app_weak.clone();
            let lg = latest_gen.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if lg.load(Ordering::Acquire) != gen_id {
                    return;
                }
                if let Some(handle) = weak.upgrade() {
                    apply_search_hits_to_ui(&handle, hits);
                }
            });
        })
    };

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
        let io = io_handle.clone();
        move |mode| {
            let pref = slint_to_theme_pref(mode);
            io.send(IoCommand::UpdateThemeConfig { theme: pref });
        }
    });

    app.global::<AppState>().on_language_changed({
        let store = session_store.clone();
        let io = io_handle.clone();
        let app_weak = app.as_weak();
        move |lang_str| {
            let pref = str_to_lang_pref(lang_str.as_str());
            io.send(IoCommand::UpdateLanguageConfig { lang: pref });
            if let Some(handle) = app_weak.upgrade() {
                refresh_session_list_ui(&handle, &store);
            }
        }
    });

    // 7.1e Measured block height feedback from Slint (`init` & `changed height`) (Fix L3, U11, X7)
    app.global::<AppState>().on_report_block_height({
        let app_weak = app.as_weak();
        move |flat_idx, measured_px| {
            if !measured_px.is_finite() || measured_px <= 1.0 {
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
                let old_slice = vp.fenwick.query_viewport(
                    vp.scroll_y,
                    vp.viewport_height,
                    VIEWPORT_OVERSCAN_PX,
                );
                let block_top_y = vp.fenwick.offset_of(idx);
                let delta = measured_px - old_h;
                let key = (
                    vp.flat_blocks[idx].msg_index,
                    vp.flat_blocks[idx].block_id.clone(),
                );
                vp.measured_heights.insert(key, measured_px);
                vp.fenwick.update_height(idx, measured_px);

                // Scroll Anchoring: compensate scroll_y when a block above the viewport resizes
                if block_top_y < vp.scroll_y && vp.scroll_y > 0.0 {
                    let max_scroll = (vp.fenwick.total_height() - vp.viewport_height).max(0.0);
                    let anchored = (vp.scroll_y + delta).clamp(0.0, max_scroll);
                    let applied_delta = anchored - vp.scroll_y;
                    vp.scroll_y = anchored;
                    if applied_delta.abs() > 0.01 {
                        LENIS_STATE.with(|lcell| {
                            let mut mgr = lcell.borrow_mut();
                            mgr.chat.current += applied_delta as f64;
                            mgr.chat.target += applied_delta as f64;
                        });
                        handle.invoke_set_chat_scroll_y(anchored);
                    }
                }

                let slice = vp.fenwick.query_viewport(
                    vp.scroll_y,
                    vp.viewport_height,
                    VIEWPORT_OVERSCAN_PX,
                );
                if slice.start_idx != old_slice.start_idx || slice.end_idx != old_slice.end_idx {
                    apply_viewport_slice(&handle, &mut vp);
                } else {
                    let state = handle.global::<AppState>();
                    state.set_top_spacer_height(slice.top_spacer_height);
                    state.set_bottom_spacer_height(slice.bottom_spacer_height);
                }
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
                vp.viewport_width = viewport_w.max(200.0);

                // Fix M4: dynamic inline wrap column budget from clamped viewport width
                let computed_cols = ((vp.viewport_width - 40.0) / 8.2).clamp(28.0, 104.0) as usize;
                let cols_changed = computed_cols.abs_diff(vp.max_cols) >= 3;
                if cols_changed {
                    vp.max_cols = computed_cols;
                    // Invalidate width-dependent offscreen measured heights & frozen cache
                    let len = vp.flat_blocks.len();
                    for i in 0..len {
                        let is_width_dependent = if vp.flat_blocks[i].is_user {
                            true
                        } else if let Some(ref sb) = vp.flat_blocks[i].source_block {
                            !matches!(
                                sb.kind,
                                BlockKind::CodeBlock { .. }
                                    | BlockKind::LatexMath { .. }
                                    | BlockKind::ThematicBreak
                            )
                        } else {
                            false
                        };
                        if is_width_dependent {
                            let key = (
                                vp.flat_blocks[i].msg_index,
                                vp.flat_blocks[i].block_id.clone(),
                            );
                            vp.measured_heights.remove(&key);
                            vp.frozen_cache.remove(&key);
                            if let Some(ref sb) = vp.flat_blocks[i].source_block {
                                let (est_h, _, _) = estimate_stream_block_height(sb, computed_cols);
                                if i < vp.fenwick.len() {
                                    vp.fenwick.update_height(i, est_h);
                                }
                            }
                        }
                    }
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

    app.global::<AppState>().on_code_block_scrolled({
        let app_weak = app.as_weak();
        move |flat_idx, scroll_x_px| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            let idx = flat_idx.max(0) as usize;
            UI_VIEWPORT_STATE.with(|cell| {
                let mut vp = cell.borrow_mut();
                let sx = scroll_x_px.max(0.0);
                vp.code_scroll_x.insert(idx, sx);
                if is_block_touched_by_selection(vp.text_selection, idx) {
                    refresh_selection_overlays(&handle, &mut vp);
                }
            });
        }
    });

    app.global::<AppState>().on_code_wrap_changed({
        let app_weak = app.as_weak();
        move |flat_idx, wrap_enabled| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            let idx = flat_idx.max(0) as usize;
            UI_VIEWPORT_STATE.with(|cell| {
                let mut vp = cell.borrow_mut();
                vp.code_wrap_states.insert(idx, wrap_enabled);
                if wrap_enabled {
                    vp.code_scroll_x.remove(&idx);
                }
                if is_block_touched_by_selection(vp.text_selection, idx) {
                    refresh_selection_overlays(&handle, &mut vp);
                }
            });
        }
    });

    // 7.2b Global Lenis (`duration = 1.2`, `lerp = 0.1`) Smooth Scroll Callbacks
    app.global::<AppState>().on_lenis_frame_tick({
        let app_weak = app.as_weak();
        move || {
            if let Some(handle) = app_weak.upgrade() {
                step_lenis_frame(&handle);
            }
        }
    });

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
                } else if delta_y_px < 0.0
                    && mgr.chat.current >= (max_scroll_px as f64 - 36.0).max(0.0)
                {
                    state.set_auto_scroll_sticky(true);
                }
            });
            ensure_lenis_timer_running(app_weak.clone());
        }
    });

    app.global::<AppState>().on_lenis_sync_chat({
        move |direct_pos_px, max_scroll_px| {
            LAST_SENT_CHAT_Y.with(|last| last.set(direct_pos_px.max(0.0)));
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
        let gen_coord = gen_coord.clone();
        move || {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            finalize_active_generation(&handle, &store, &records, &gen_coord);
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
        let gen_coord = gen_coord.clone();
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
            finalize_active_generation(&handle, &store, &records, &gen_coord);
            load_session_into_ui(&handle, &store, &records, sid);
        }
    });

    app.global::<AppState>().on_clone_session({
        let app_weak = app.as_weak();
        let store = session_store.clone();
        let records = records.clone();
        let gen_coord = gen_coord.clone();
        move |sid_str| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            let Ok(sid) = UuidV7::parse_str(sid_str.as_str()) else {
                return;
            };
            finalize_active_generation(&handle, &store, &records, &gen_coord);
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
        let gen_coord = gen_coord.clone();
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
                    finalize_active_generation(&handle, &store, &records, &gen_coord);
                    let next_id = store
                        .list_sessions(false)
                        .first()
                        .map(|m| m.id)
                        .or_else(|| store.create_session(None).ok().map(|m| m.id));
                    if let Some(nid) = next_id {
                        load_session_into_ui(&handle, &store, &records, nid);
                    } else {
                        eprintln!("[fugue-ui] Failed to create fallback session after archive");
                        refresh_session_list_ui(&handle, &store);
                    }
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
        let gen_coord = gen_coord.clone();
        move |sid_str| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            let Ok(sid) = UuidV7::parse_str(sid_str.as_str()) else {
                return;
            };
            let deleting_current = handle.global::<AppState>().get_current_session_id() == sid_str;
            if deleting_current {
                finalize_active_generation(&handle, &store, &records, &gen_coord);
            }
            if store.delete_session(sid).is_ok() {
                if deleting_current {
                    let show_archived = handle.global::<AppState>().get_show_archived_sessions();
                    let next_id = store
                        .list_sessions(show_archived)
                        .first()
                        .map(|m| m.id)
                        .or_else(|| store.list_sessions(false).first().map(|m| m.id))
                        .or_else(|| {
                            handle
                                .global::<AppState>()
                                .set_show_archived_sessions(false);
                            store.create_session(None).ok().map(|m| m.id)
                        });
                    if let Some(nid) = next_id {
                        load_session_into_ui(&handle, &store, &records, nid);
                    } else {
                        eprintln!("[fugue-ui] Failed to create fallback session after delete");
                        refresh_session_list_ui(&handle, &store);
                    }
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
        let io = io_handle.clone();
        let latest_gen = io_latest_gen.clone();
        move |query| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            let q = query.to_string();
            let gen_id = io.request_search(q.clone());
            latest_gen.store(gen_id, Ordering::Release);
            if q.trim().is_empty() {
                handle
                    .global::<AppState>()
                    .set_search_hits(ModelRc::default());
            }
        }
    });

    app.global::<AppState>().on_jump_to_search_hit({
        let app_weak = app.as_weak();
        let store = session_store.clone();
        let records = records.clone();
        let gen_coord = gen_coord.clone();
        move |sid_str, mid_str, msg_idx_hint| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            let Ok(sid) = UuidV7::parse_str(sid_str.as_str()) else {
                return;
            };
            let target_mid = UuidV7::parse_str(mid_str.as_str()).ok();

            if handle.global::<AppState>().get_current_session_id() != sid_str {
                finalize_active_generation(&handle, &store, &records, &gen_coord);
                load_session_into_ui(&handle, &store, &records, sid);
            }

            // Locate message index by UUIDv7 (or fall back to msg_idx_hint)
            let resolved_msg_idx = {
                let recs = records.read();
                target_mid
                    .and_then(|mid| recs.iter().position(|r| r.message_id == mid))
                    .unwrap_or_else(|| (msg_idx_hint.max(0) as usize).min(recs.len().saturating_sub(1)))
            };

            // Compute exact vertical pixel offset via Fenwick tree prefix sum and highlight target message
            let scroll_target = UI_VIEWPORT_STATE.with(|cell| {
                let mut vp = cell.borrow_mut();
                let matching_flats: Vec<usize> = vp
                    .flat_blocks
                    .iter()
                    .filter(|b| b.msg_index == resolved_msg_idx)
                    .map(|b| b.flat_index)
                    .collect();
                let first_flat = matching_flats.first().copied();
                let last_flat = matching_flats.last().copied();
                if let (Some(f), Some(l)) = (first_flat, last_flat) {
                    if let Some(last_geom) = get_or_compile_geometry(l, &mut vp) {
                        let (_, end_pos) = expand_block_range(l, &last_geom);
                        vp.text_selection = Some(TextSelection {
                            anchor: TextPos::new(f, 0, 0),
                            focus: end_pos,
                        });
                    }
                } else {
                    vp.text_selection = None;
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

    // 7.3 Global Chrome-style Character-Level Selection Engine
    fn hit_test_viewport_point(vp: &mut UiViewportState, vx: f32, vy: f32) -> Option<TextPos> {
        let doc_y = (vy + vp.scroll_y).max(0.0);
        let (flat_idx, block_top_y) = vp.fenwick.find_index_at_y(doc_y)?;
        let geom = get_or_compile_geometry(flat_idx, vp)?;
        Some(hit_test_block(flat_idx, &geom, vx, doc_y - block_top_y))
    }

    app.global::<AppState>().on_chat_pointer_down({
        let app_weak = app.as_weak();

        move |vx, vy, button, shift, _ctrl| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            UI_VIEWPORT_STATE.with(|cell| {
                let mut vp = cell.borrow_mut();
                if button == 1 {
                    let doc_y = (vy + vp.scroll_y).max(0.0);
                    if let Some((flat_idx, _)) = vp.fenwick.find_index_at_y(doc_y) {
                        if let Some((msg_index, is_table)) = vp
                            .flat_blocks
                            .get(flat_idx)
                            .map(|e| (e.msg_index, e.is_table))
                        {
                            vp.context_target_flat = Some(flat_idx);
                            vp.context_target_msg = Some(msg_index);
                            handle.global::<AppState>().set_context_menu_is_table(is_table);
                            handle.global::<AppState>().set_context_menu_open(true);
                        }
                    }
                    return;
                }

                let Some(pos) = hit_test_viewport_point(&mut vp, vx, vy) else {
                    vp.text_selection = None;
                    refresh_selection_overlays(&handle, &mut vp);
                    return;
                };

                let click_count = vp.drag_gesture.register_press(vx, vy);
                if shift {
                    let anchor = vp.text_selection.map(|s| s.anchor).unwrap_or(pos);
                    vp.text_selection = Some(TextSelection { anchor, focus: pos });
                } else if click_count == 2 {
                    if let Some(geom) = get_or_compile_geometry(pos.flat_index, &mut vp) {
                        let (s, e) = expand_word_at(&geom, pos);
                        vp.text_selection = Some(TextSelection { anchor: s, focus: e });
                    }
                } else if click_count == 3 {
                    if let Some(geom) = get_or_compile_geometry(pos.flat_index, &mut vp) {
                        let (s, e) = expand_block_range(pos.flat_index, &geom);
                        vp.text_selection = Some(TextSelection { anchor: s, focus: e });
                    }
                } else {
                    vp.text_selection = Some(TextSelection {
                        anchor: pos,
                        focus: pos,
                    });
                }
                refresh_selection_overlays(&handle, &mut vp);
            });
        }
    });

    app.global::<AppState>().on_chat_pointer_move({
        let app_weak = app.as_weak();

        move |vx, vy, pressed| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            if !pressed {
                return;
            }
            UI_VIEWPORT_STATE.with(|cell| {
                let mut vp = cell.borrow_mut();
                if !vp.drag_gesture.update_move(vx, vy) {
                    return;
                }
                if let Some(pos) = hit_test_viewport_point(&mut vp, vx, vy) {
                    let anchor = vp.text_selection.map(|s| s.anchor).unwrap_or(pos);
                    if vp.text_selection.map(|s| s.focus) != Some(pos) {
                        vp.text_selection = Some(TextSelection { anchor, focus: pos });
                        refresh_selection_overlays(&handle, &mut vp);
                    }
                }
            });
        }
    });

    app.global::<AppState>().on_chat_pointer_up({
        let app_weak = app.as_weak();

        move |_vx, _vy, _button| {
            let Some(_handle) = app_weak.upgrade() else {
                return;
            };
            UI_VIEWPORT_STATE.with(|cell| {
                cell.borrow_mut().drag_gesture.release();
            });
        }
    });

    // 7.4 16ms Timer-driven Edge Auto-Scroll (works even when pointer is stationary)
    app.global::<AppState>().on_selection_auto_scroll_tick({
        let app_weak = app.as_weak();

        move |vy| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            UI_VIEWPORT_STATE.with(|cell| {
                let mut vp = cell.borrow_mut();
                if !vp.drag_gesture.is_dragging {
                    return;
                }
                let vh = vp.viewport_height;
                let max_scroll = (vp.fenwick.total_height() - vh).max(0.0);
                if max_scroll <= 0.0 {
                    return;
                }

                let edge = 36.0f32;
                let step = if vy < edge && vp.scroll_y > 0.0 {
                    -((edge - vy).clamp(2.0, 28.0) * 0.65)
                } else if vy > vh - edge && vp.scroll_y < max_scroll {
                    (vy - (vh - edge)).clamp(2.0, 28.0) * 0.65
                } else {
                    0.0
                };

                if step.abs() <= 0.01 {
                    return;
                }

                let next_scroll = (vp.scroll_y + step).clamp(0.0, max_scroll);
                if (next_scroll - vp.scroll_y).abs() <= 0.01 {
                    return;
                }

                if step < 0.0 {
                    handle.global::<AppState>().set_auto_scroll_sticky(false);
                }
                vp.scroll_y = next_scroll;
                LENIS_STATE.with(|lcell| {
                    let mut mgr = lcell.borrow_mut();
                    mgr.chat.jump_to(next_scroll as f64, max_scroll as f64);
                });
                handle.invoke_set_chat_scroll_y(next_scroll);

                let vx = vp.drag_gesture.last_viewport_x;
                if let Some(pos) = hit_test_viewport_point(&mut vp, vx, vy) {
                    let anchor = vp.text_selection.map(|s| s.anchor).unwrap_or(pos);
                    vp.text_selection = Some(TextSelection { anchor, focus: pos });
                }
                apply_viewport_slice(&handle, &mut vp);
            });
        }
    });

    // 7.5 Singleton Context Menu Action Dispatch (Unified Character Range)
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
                        let payload = serialize_blocks_for_clipboard(&mut vp, act);
                        if !payload.is_empty() {
                            if let Ok(mut clipboard) = arboard::Clipboard::new() {
                                let _ = clipboard.set_text(payload);
                            }
                        }
                    }
                    "select_block" => {
                        if let Some(target) = vp.context_target_flat {
                            if let Some(geom) = get_or_compile_geometry(target, &mut vp) {
                                let (s, e) = expand_block_range(target, &geom);
                                vp.text_selection = Some(TextSelection { anchor: s, focus: e });
                                refresh_selection_overlays(&handle, &mut vp);
                            }
                        }
                    }
                    "select_message" => {
                        if let Some(target_msg) = vp.context_target_msg {
                            let matching: Vec<usize> = vp
                                .flat_blocks
                                .iter()
                                .filter(|b| b.msg_index == target_msg)
                                .map(|b| b.flat_index)
                                .collect();
                            if let (Some(&f), Some(&l)) = (matching.first(), matching.last()) {
                                if let Some(last_geom) = get_or_compile_geometry(l, &mut vp) {
                                    let (_, end_pos) = expand_block_range(l, &last_geom);
                                    vp.text_selection = Some(TextSelection {
                                        anchor: TextPos::new(f, 0, 0),
                                        focus: end_pos,
                                    });
                                    refresh_selection_overlays(&handle, &mut vp);
                                }
                            }
                        }
                    }
                    "select_all" => {
                        let len = vp.flat_blocks.len();
                        if len > 0 {
                            let last = len - 1;
                            if let Some(last_geom) = get_or_compile_geometry(last, &mut vp) {
                                let (_, end_pos) = expand_block_range(last, &last_geom);
                                vp.text_selection = Some(TextSelection {
                                    anchor: TextPos::new(0, 0, 0),
                                    focus: end_pos,
                                });
                                refresh_selection_overlays(&handle, &mut vp);
                            }
                        }
                    }
                    "clear_selection" => {
                        vp.text_selection = None;
                        refresh_selection_overlays(&handle, &mut vp);
                    }
                    _ => {}
                }
            });
        }
    });

    // 7.6b Safe External URL Opening & Task Checkbox Toggle
    app.global::<AppState>().on_open_external_url(|url| {
        if !open_external_url_safe(url.as_str()) {
            // Fallback to clipboard when scheme is unsupported
            if let Ok(mut cb) = arboard::Clipboard::new() {
                let _ = cb.set_text(url.to_string());
            }
        }
    });

    app.global::<AppState>().on_toggle_task_checkbox({
        let records = records.clone();
        let app_weak = app.as_weak();
        move |flat_idx, item_idx| {
            let Some(handle) = app_weak.upgrade() else {
                return;
            };
            let idx = flat_idx.max(0) as usize;
            let item_u = item_idx.max(0) as usize;
            let target = UI_VIEWPORT_STATE.with(|cell| {
                let vp = cell.borrow();
                vp.flat_blocks
                    .get(idx)
                    .map(|b| (b.msg_index, b.block_id.clone(), b.raw_markdown.clone()))
            });
            let Some((msg_idx, block_id, old_md)) = target else {
                return;
            };
            let Some(new_md) = toggle_task_checkbox_in_markdown(&old_md, item_u) else {
                return;
            };
            {
                let mut recs = records.write();
                if let Some(rec) = recs.get_mut(msg_idx) {
                    if let Some(pos) = rec.accumulated_content.find(&old_md) {
                        rec.accumulated_content
                            .replace_range(pos..pos + old_md.len(), &new_md);
                        let mut auto = StreamBlockAutomaton::new();
                        auto.advance(&rec.accumulated_content, !rec.is_streaming);
                        *rec.automaton.write() = auto;
                    }
                }
                UI_VIEWPORT_STATE.with(|cell| {
                    let mut vp = cell.borrow_mut();
                    vp.frozen_cache.remove(&(msg_idx, block_id.clone()));
                    vp.measured_heights.remove(&(msg_idx, block_id));
                    vp.synced_msg_count = 0;
                });
                let is_dark = handle.global::<Theme>().get_dark();
                sync_and_cull_ui(&handle, &recs, is_dark);
            }
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
        let gen_coord = gen_coord.clone();
        let records = records.clone();
        let store = session_store.clone();
        let app_weak = app.as_weak();

        move || {
            if let Some(handle) = app_weak.upgrade() {
                finalize_active_generation(&handle, &store, &records, &gen_coord);
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
        let gen_coord = gen_coord.clone();
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
            if handle.global::<AppState>().get_is_generating() {
                return;
            }
            let is_dark = handle.global::<Theme>().get_dark();

            // Resolve active session ID (or create a session if none exists)
            let active_sid = match UuidV7::parse_str(
                handle.global::<AppState>().get_current_session_id().as_str(),
            ) {
                Ok(id) => id,
                Err(_) => match store.create_session(None) {
                    Ok(meta) => {
                        handle
                            .global::<AppState>()
                            .set_current_session_id(SharedString::from(meta.id.to_string()));
                        meta.id
                    }
                    Err(e) => {
                        eprintln!("[fugue-ui] Failed to create session on send: {}", e);
                        return;
                    }
                },
            };

            // Immediately persist user message into the isolated session sandbox
            let user_msg_id = store
                .append_message(active_sid, MessageRole::User, &user_str, Vec::new())
                .map(|m| m.id)
                .unwrap_or_else(|_| UuidV7::new_now());

            refresh_session_list_ui(&handle, &store);

            // Preserve user's active historical text selection across streaming turns
            let (my_gen_id, mut cancel_rx) = gen_coord.begin_generation();
            handle.global::<AppState>().set_has_messages(true);
            handle.global::<AppState>().set_is_generating(true);

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
            let gen_coord_clone = gen_coord.clone();
            let omni_clone = omni.clone();
            let store_clone = store.clone();
            let gateway_clone = gateway.clone();
            let turn_session_id = active_sid;

            let outer_handle = rt_handle.spawn(async move {
                let config = match DeepSeekConfig::load_from_env() {
                    Ok(c) => c,
                    Err(e) => {
                        let store_err = store_clone.clone();
                        let gen_check = gen_coord_clone.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if !gen_check.is_current(my_gen_id) {
                                return;
                            }
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

                let stream_cancel = cancel_rx.clone();
                let stream_handle = tokio::spawn(async move {
                    client
                        .stream_chat(&config, prompt_messages, stream_cancel, tx)
                        .await;
                });
                gen_coord_clone.register_task(my_gen_id, stream_handle.abort_handle());

                // 16ms frame-throttled streaming batcher (`session-update-batcher` equivalent)
                let mut pending_delta = String::new();
                let mut flush_deadline: Option<tokio::time::Instant> = None;
                let frame_interval = Duration::from_millis(16);
                let mut finished = false;

                loop {
                    if !gen_coord_clone.is_current(my_gen_id) || *cancel_rx.borrow() {
                        if !pending_delta.is_empty() {
                            let tail_chunk = std::mem::take(&mut pending_delta);
                            let mut recs = records_clone.write();
                            if let Some(last) = recs.last_mut() {
                                last.accumulated_content.push_str(&tail_chunk);
                            }
                        }
                        finished = true;
                        break;
                    }

                    let next_event: Option<StreamEvent> = tokio::select! {
                        biased;
                        _ = cancel_rx.changed() => {
                            if !pending_delta.is_empty() {
                                let tail_chunk = std::mem::take(&mut pending_delta);
                                let mut recs = records_clone.write();
                                if let Some(last) = recs.last_mut() {
                                    last.accumulated_content.push_str(&tail_chunk);
                                }
                            }
                            finished = true;
                            break;
                        }
                        _ = async {
                            if let Some(dl) = flush_deadline {
                                tokio::time::sleep_until(dl).await;
                            } else {
                                std::future::pending::<()>().await;
                            }
                        }, if flush_deadline.is_some() => {
                            flush_deadline = None;
                            if !pending_delta.is_empty() && gen_coord_clone.is_current(my_gen_id) {
                                let chunk = std::mem::take(&mut pending_delta);
                                let recs_lock = records_clone.clone();
                                let weak_app = app_weak_clone.clone();
                                let gen_check = gen_coord_clone.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if !gen_check.is_current(my_gen_id) {
                                        return;
                                    }
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
                            continue;
                        }
                        ev = rx.recv() => ev,
                    };

                    let Some(event) = next_event else {
                        break;
                    };

                    match event {
                        StreamEvent::ContentDelta(delta) => {
                            pending_delta.push_str(&delta);
                            let now = tokio::time::Instant::now();
                            let dl = *flush_deadline.get_or_insert(now + frame_interval);
                            if now >= dl {
                                flush_deadline = None;
                                let chunk = std::mem::take(&mut pending_delta);
                                let recs_lock = records_clone.clone();
                                let weak_app = app_weak_clone.clone();
                                let gen_check = gen_coord_clone.clone();

                                let _ = slint::invoke_from_event_loop(move || {
                                    if !gen_check.is_current(my_gen_id) {
                                        return;
                                    }
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
                            flush_deadline = None;
                            let flushed_chunk = std::mem::take(&mut pending_delta);
                            let recs_lock = records_clone.clone();
                            let weak_app = app_weak_clone.clone();
                            let omni = omni_clone.clone();
                            let gw = gateway_clone.clone();
                            let gen_check = gen_coord_clone.clone();

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
                                if !gen_check.is_current(my_gen_id) {
                                    return;
                                }
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
                            finished = true;
                            let flushed_chunk = std::mem::take(&mut pending_delta);
                            let recs_lock = records_clone.clone();
                            let weak_app = app_weak_clone.clone();
                            let store_done = store_clone.clone();
                            let gen_check = gen_coord_clone.clone();

                            let _ = slint::invoke_from_event_loop(move || {
                                if !gen_check.is_current(my_gen_id) {
                                    return;
                                }
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
                            finished = true;
                            let flushed_chunk = std::mem::take(&mut pending_delta);
                            let recs_lock = records_clone.clone();
                            let weak_app = app_weak_clone.clone();
                            let store_err = store_clone.clone();
                            let gen_check = gen_coord_clone.clone();

                            let _ = slint::invoke_from_event_loop(move || {
                                if !gen_check.is_current(my_gen_id) {
                                    return;
                                }
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

                // Safety net: if stream channel closed unexpectedly without Done/Error, still reset generating state
                if !finished && gen_coord_clone.is_current(my_gen_id) {
                    let flushed_chunk = std::mem::take(&mut pending_delta);
                    let recs_lock = records_clone.clone();
                    let weak_app = app_weak_clone.clone();
                    let store_fallback = store_clone.clone();
                    let gen_check = gen_coord_clone.clone();

                    let _ = slint::invoke_from_event_loop(move || {
                        if !gen_check.is_current(my_gen_id) {
                            return;
                        }
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
                                        &store_fallback,
                                        turn_session_id,
                                        last,
                                    );
                                }
                                let is_dark = h.global::<Theme>().get_dark();
                                sync_and_cull_ui(&h, &recs, is_dark);
                            }
                            refresh_session_list_ui(&h, &store_fallback);
                            h.invoke_scroll_to_bottom_if_sticky();
                        }
                    });
                }
            });
            gen_coord.register_task(my_gen_id, outer_handle.abort_handle());
        }
    });

    app.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_coordinator_invalidates_stale_turns_and_notifies_cancel() {
        let coord = GenerationCoordinator::new();
        let (gen1, rx1) = coord.begin_generation();
        assert!(coord.is_current(gen1));
        assert!(!*rx1.borrow());

        let (gen2, rx2) = coord.begin_generation();
        assert_ne!(gen1, gen2);
        assert!(!coord.is_current(gen1));
        assert!(coord.is_current(gen2));
        assert!(*rx1.borrow());
        assert!(!*rx2.borrow());

        coord.cancel_active();
        assert!(!coord.is_current(gen2));
        assert!(*rx2.borrow());
    }

    #[test]
    fn external_url_validation_allows_only_http_https_without_whitespace() {
        assert!(is_safe_http_url("https://example.com/path?a=1&b=2"));
        assert!(is_safe_http_url("http://localhost:8080"));
        assert!(!is_safe_http_url("javascript:alert(1)"));
        assert!(!is_safe_http_url("file:///C:/Windows/System32/cmd.exe"));
        assert!(!is_safe_http_url("https://example.com/bad url"));
    }

    #[test]
    fn toggle_task_checkbox_flips_target_item_only() {
        let md = "- [ ] first\n- [x] second\n- [ ] third";
        let toggled = toggle_task_checkbox_in_markdown(md, 1).expect("should toggle second");
        assert_eq!(toggled, "- [ ] first\n- [ ] second\n- [ ] third");
        let toggled2 = toggle_task_checkbox_in_markdown(&toggled, 2).expect("should toggle third");
        assert_eq!(toggled2, "- [ ] first\n- [ ] second\n- [x] third");
    }
}
