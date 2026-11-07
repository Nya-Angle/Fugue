//! # fugue-core
//! Core domain models, RFC 9562 UUIDv7 monotonic generator, protocol schemas,
//! LLM-reserved client control protocol, and common error types for Fugue.

use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Global packed monotonic state for RFC 9562 UUIDv7 generation:
/// High 48 bits = last Unix timestamp in milliseconds; Low 16 bits = monotonic sequence counter.
static UUID_V7_STATE: AtomicU64 = AtomicU64::new(0);

/// RFC 9562 time-ordered monotonic 128-bit UUIDv7 identifier.
/// Guarantees strict lexicographical and binary (`u128`) monotonicity even within the same millisecond.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct UuidV7([u8; 16]);

impl UuidV7 {
    /// Generates a new strictly monotonic RFC 9562 UUIDv7.
    #[inline]
    pub fn new_now() -> Self {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(1)
            & 0x0000_FFFF_FFFF_FFFF;

        let (ts_ms, seq_12) = loop {
            let prev = UUID_V7_STATE.load(Ordering::Relaxed);
            let prev_ms = prev >> 16;
            let prev_seq = prev & 0xFFFF;

            let (next_ms, next_seq) = if now_ms > prev_ms {
                (now_ms, 0u64)
            } else {
                // Same millisecond or clock drift: increment monotonic counter.
                // If 12-bit counter overflows (4096 IDs/ms), advance logical millisecond by 1.
                let s = prev_seq + 1;
                if s < 0x1000 {
                    (prev_ms, s)
                } else {
                    (prev_ms + 1, 0)
                }
            };

            let next_packed = (next_ms << 16) | (next_seq & 0xFFFF);
            if UUID_V7_STATE
                .compare_exchange_weak(prev, next_packed, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
            {
                break (next_ms, (next_seq as u16) & 0x0FFF);
            }
        };

        // Fast SplitMix64 entropy mixed with thread address, timestamp, and sequence
        let mut entropy = ts_ms
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add((seq_12 as u64) << 32)
            ^ ( (&ts_ms as *const u64 as usize as u64).rotate_left(17) );
        entropy = (entropy ^ (entropy >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        entropy = (entropy ^ (entropy >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        let rand_b = entropy ^ (entropy >> 31);

        let mut bytes = [0u8; 16];
        // 48-bit big-endian unix_ts_ms
        bytes[0] = ((ts_ms >> 40) & 0xFF) as u8;
        bytes[1] = ((ts_ms >> 32) & 0xFF) as u8;
        bytes[2] = ((ts_ms >> 24) & 0xFF) as u8;
        bytes[3] = ((ts_ms >> 16) & 0xFF) as u8;
        bytes[4] = ((ts_ms >> 8) & 0xFF) as u8;
        bytes[5] = (ts_ms & 0xFF) as u8;

        // 4-bit version (0b0111 = 7) + 12-bit monotonic rand_a (seq_12)
        bytes[6] = 0x70 | (((seq_12 >> 8) & 0x0F) as u8);
        bytes[7] = (seq_12 & 0xFF) as u8;

        // 2-bit RFC 9562 variant (0b10) + 62-bit rand_b
        let rb_bytes = rand_b.to_be_bytes();
        bytes[8] = (rb_bytes[0] & 0x3F) | 0x80;
        bytes[9..16].copy_from_slice(&rb_bytes[1..8]);

        Self(bytes)
    }

    #[inline]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    #[inline]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    #[inline]
    pub const fn into_bytes(self) -> [u8; 16] {
        self.0
    }

    #[inline]
    pub const fn as_u128(&self) -> u128 {
        u128::from_be_bytes(self.0)
    }

    #[inline]
    pub const fn from_u128(val: u128) -> Self {
        Self(val.to_be_bytes())
    }

    /// Extracts the embedded 48-bit Unix millisecond timestamp from the UUIDv7.
    #[inline]
    pub const fn timestamp_ms(&self) -> u64 {
        ((self.0[0] as u64) << 40)
            | ((self.0[1] as u64) << 32)
            | ((self.0[2] as u64) << 24)
            | ((self.0[3] as u64) << 16)
            | ((self.0[4] as u64) << 8)
            | (self.0[5] as u64)
    }

    /// Parses a standard 36-char hyphenated or 32-char hex UUID string in O(1) with zero heap allocations.
    pub fn parse_str(input: &str) -> Result<Self> {
        let s = input.trim();
        let mut out = [0u8; 16];
        let bytes = s.as_bytes();

        if bytes.len() == 36 {
            if bytes[8] != b'-' || bytes[13] != b'-' || bytes[18] != b'-' || bytes[23] != b'-' {
                return Err(FugueError::InvalidParameter(format!(
                    "Invalid UUID format: '{}'",
                    input
                )));
            }
            const POS: [usize; 16] = [
                0, 2, 4, 6, 9, 11, 14, 16, 19, 21, 24, 26, 28, 30, 32, 34,
            ];
            for (i, &p) in POS.iter().enumerate() {
                out[i] = decode_hex_byte(bytes[p], bytes[p + 1]).ok_or_else(|| {
                    FugueError::InvalidParameter(format!("Invalid hex in UUID: '{}'", input))
                })?;
            }
            Ok(Self(out))
        } else if bytes.len() == 32 {
            for i in 0..16 {
                out[i] = decode_hex_byte(bytes[i * 2], bytes[i * 2 + 1]).ok_or_else(|| {
                    FugueError::InvalidParameter(format!("Invalid hex in UUID: '{}'", input))
                })?;
            }
            Ok(Self(out))
        } else {
            Err(FugueError::InvalidParameter(format!(
                "Invalid UUID length ({}): '{}'",
                bytes.len(),
                input
            )))
        }
    }
}

#[inline]
const fn decode_hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[inline]
const fn decode_hex_byte(hi: u8, lo: u8) -> Option<u8> {
    match (decode_hex_nibble(hi), decode_hex_nibble(lo)) {
        (Some(h), Some(l)) => Some((h << 4) | l),
        _ => None,
    }
}

impl fmt::Display for UuidV7 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut buf = [b'-'; 36];
        const POS: [usize; 16] = [
            0, 2, 4, 6, 9, 11, 14, 16, 19, 21, 24, 26, 28, 30, 32, 34,
        ];
        for (i, &b) in self.0.iter().enumerate() {
            let p = POS[i];
            buf[p] = HEX[(b >> 4) as usize];
            buf[p + 1] = HEX[(b & 0x0F) as usize];
        }
        // Safety: buf contains only valid ASCII hex digits and hyphens
        let s = unsafe { std::str::from_utf8_unchecked(&buf) };
        f.write_str(s)
    }
}

impl fmt::Debug for UuidV7 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UuidV7({})", self)
    }
}

impl FromStr for UuidV7 {
    type Err = FugueError;
    fn from_str(s: &str) -> Result<Self> {
        Self::parse_str(s)
    }
}

/// Task complexity tier conforming to AXIOM[01]: DYNAMIC_RANGE_INVARIANCE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskComplexity {
    /// L0: Overhead == 0, single instance direct pass, millisecond latency.
    L0,
    /// L1: Dynamic mesh topology, multi-avatar coordination.
    L1,
    /// L2: Complex multi-stage mesh with speculative planning and action concurrency.
    L2,
}

/// Task status lifecycle state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Unclaimed,
    Leased,
    Running,
    Review,
    Committed,
    Failed,
}

/// Action category for SEMANTIC_ACTION_v2 feedback protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionCategory {
    CodeMutation,
    CommandExecution,
    ContentGeneration,
}

/// Structured telemetry feedback according to SEMANTIC_ACTION_v2 schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticActionFeedback {
    pub action_category: ActionCategory,
    pub target_subject: String,
    pub mutation_rationale: String,
    pub changed_parameters: Vec<String>,
    pub environmental_side_effects: Vec<String>,
    pub speculative_next: String,
}

/// Role of a persisted conversation message.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageRole {
    User = 0,
    Assistant = 1,
    System = 2,
}

impl MessageRole {
    #[inline]
    pub const fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::User,
            1 => Self::Assistant,
            _ => Self::System,
        }
    }

    #[inline]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::System => "system",
        }
    }
}

/// Persisted action capsule inside an assistant message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredActionCapsule {
    pub summary: String,
    pub detail: String,
    pub is_expanded: bool,
}

/// Persisted message record with globally unique UUIDv7 identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredMessageRecord {
    pub id: UuidV7,
    pub session_id: UuidV7,
    pub role: MessageRole,
    pub content: String,
    pub created_at_ms: u64,
    pub capsules: Vec<StoredActionCapsule>,
}

/// Metadata summary for a sandboxed session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub id: UuidV7,
    pub title: String,
    pub pinned: bool,
    pub archived: bool,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub message_count: u32,
    pub preview_snippet: String,
    /// True if the user or LLM explicitly renamed this session (disables auto-title override).
    pub custom_titled: bool,
}

/// Client theme preference persisted in `C:\Users\<Username>\.fugue\config.bin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreference {
    System,
    Dark,
    Light,
}

/// Client language preference persisted in `C:\Users\<Username>\.fugue\config.bin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LanguagePreference {
    Zh,
    En,
}

impl LanguagePreference {
    #[inline]
    pub const fn as_code(&self) -> &'static str {
        match self {
            Self::Zh => "zh",
            Self::En => "en",
        }
    }
}

/// Search match result returned by the SIMD + N-Gram cross-session indexer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchMatchHit {
    pub session_id: UuidV7,
    pub session_title: String,
    pub message_id: UuidV7,
    pub message_index: usize,
    pub role: MessageRole,
    pub created_at_ms: u64,
    pub matched_snippet: String,
    pub byte_offset_in_msg: usize,
}

/// Result of inspecting a specific Session ID or Message ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "entity_type", rename_all = "snake_case")]
pub enum InspectEntityResult {
    Session {
        metadata: SessionMetadata,
        sandbox_path: String,
        messages: Vec<StoredMessageRecord>,
    },
    Message {
        session_metadata: SessionMetadata,
        message_index: usize,
        message: StoredMessageRecord,
    },
}

/// Reserved Client Control Commands for both UI actions and future LLM tool invocations.
/// Covers all 9 required client operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum ClientControlCommand {
    /// 1. Clone an existing session into a completely independent new session sandbox.
    CloneSession {
        session_id: String,
        new_title: Option<String>,
    },
    /// 2. Rename a session.
    RenameSession {
        session_id: String,
        new_title: String,
    },
    /// 3. Pin or unpin a session.
    SetSessionPinned {
        session_id: String,
        pinned: bool,
    },
    /// 4. Archive or unarchive a session.
    SetSessionArchived {
        session_id: String,
        archived: bool,
    },
    /// 5. Permanently delete a session and its filesystem sandbox.
    DeleteSession {
        session_id: String,
    },
    /// 6. High-speed content search across one, multiple, or all sessions.
    SearchSessions {
        /// `None` or empty = search all sessions; `Some(vec![...])` = search specified session IDs.
        session_ids: Option<Vec<String>>,
        query: String,
        limit: Option<usize>,
    },
    /// 7. Look up a specific session or message record by its unique UUIDv7 ID.
    InspectById {
        id: String,
    },
    /// 8. Switch client theme mode (`dark`, `light`, `system`).
    SetThemeMode {
        mode: ThemePreference,
    },
    /// 9. Switch client interface language (`zh`, `en`).
    SetLanguage {
        lang: LanguagePreference,
    },
}

impl ClientControlCommand {
    /// Parses an LLM function call (`tool_name`, `arguments_json`) into a typed `ClientControlCommand`.
    pub fn from_tool_call(tool_name: &str, arguments_json: &str) -> Result<Self> {
        let val: serde_json::Value = serde_json::from_str(arguments_json)?;
        match tool_name {
            "clone_session" => {
                let session_id = val
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| FugueError::InvalidParameter("Missing 'session_id'".into()))?
                    .to_string();
                let new_title = val
                    .get("new_title")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                Ok(Self::CloneSession {
                    session_id,
                    new_title,
                })
            }
            "rename_session" => {
                let session_id = val
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| FugueError::InvalidParameter("Missing 'session_id'".into()))?
                    .to_string();
                let new_title = val
                    .get("new_title")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| FugueError::InvalidParameter("Missing 'new_title'".into()))?
                    .to_string();
                Ok(Self::RenameSession {
                    session_id,
                    new_title,
                })
            }
            "set_session_pinned" => {
                let session_id = val
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| FugueError::InvalidParameter("Missing 'session_id'".into()))?
                    .to_string();
                let pinned = val
                    .get("pinned")
                    .and_then(|v| v.as_bool())
                    .ok_or_else(|| FugueError::InvalidParameter("Missing boolean 'pinned'".into()))?;
                Ok(Self::SetSessionPinned { session_id, pinned })
            }
            "set_session_archived" => {
                let session_id = val
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| FugueError::InvalidParameter("Missing 'session_id'".into()))?
                    .to_string();
                let archived = val.get("archived").and_then(|v| v.as_bool()).ok_or_else(|| {
                    FugueError::InvalidParameter("Missing boolean 'archived'".into())
                })?;
                Ok(Self::SetSessionArchived {
                    session_id,
                    archived,
                })
            }
            "delete_session" => {
                let session_id = val
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| FugueError::InvalidParameter("Missing 'session_id'".into()))?
                    .to_string();
                Ok(Self::DeleteSession { session_id })
            }
            "search_sessions" => {
                let query = val
                    .get("query")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| FugueError::InvalidParameter("Missing 'query'".into()))?
                    .to_string();
                let session_ids = val.get("session_ids").and_then(|v| v.as_array()).map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                });
                let limit = val.get("limit").and_then(|v| v.as_u64()).map(|n| n as usize);
                Ok(Self::SearchSessions {
                    session_ids,
                    query,
                    limit,
                })
            }
            "inspect_by_id" => {
                let id = val
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| FugueError::InvalidParameter("Missing 'id'".into()))?
                    .to_string();
                Ok(Self::InspectById { id })
            }
            "set_theme_mode" => {
                let mode_str = val
                    .get("mode")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| FugueError::InvalidParameter("Missing 'mode'".into()))?;
                let mode = match mode_str.to_ascii_lowercase().as_str() {
                    "dark" => ThemePreference::Dark,
                    "light" => ThemePreference::Light,
                    "system" => ThemePreference::System,
                    _ => {
                        return Err(FugueError::InvalidParameter(format!(
                            "Invalid theme mode '{}', expected 'dark', 'light', or 'system'",
                            mode_str
                        )))
                    }
                };
                Ok(Self::SetThemeMode { mode })
            }
            "set_language" => {
                let lang_str = val
                    .get("lang")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| FugueError::InvalidParameter("Missing 'lang'".into()))?;
                let lang = match lang_str.to_ascii_lowercase().as_str() {
                    "zh" | "zh-cn" | "chinese" => LanguagePreference::Zh,
                    "en" | "en-us" | "english" => LanguagePreference::En,
                    _ => {
                        return Err(FugueError::InvalidParameter(format!(
                            "Invalid language '{}', expected 'zh' or 'en'",
                            lang_str
                        )))
                    }
                };
                Ok(Self::SetLanguage { lang })
            }
            other => Err(FugueError::InvalidParameter(format!(
                "Unknown client control tool '{}'",
                other
            ))),
        }
    }
}

/// Structured response returned by `ClientControlCommand` execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ClientControlResponse {
    SessionCreatedOrUpdated {
        session: SessionMetadata,
    },
    SessionDeleted {
        deleted_session_id: UuidV7,
    },
    SearchResults {
        query: String,
        hits: Vec<SearchMatchHit>,
    },
    Inspected {
        result: Box<InspectEntityResult>,
    },
    SettingsUpdated {
        theme: ThemePreference,
        lang: LanguagePreference,
    },
}

/// Returns standard OpenAI/DeepSeek Function-Calling JSON Schemas for all 9 reserved LLM client operations.
/// Future teams can pass this directly to authorized sub-agents or tool routers.
pub fn client_control_tool_schemas() -> Vec<serde_json::Value> {
    serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "clone_session",
                "description": "Clone an existing session into a completely independent new session sandbox with new UUIDv7 identifiers.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "session_id": { "type": "string", "description": "Source session UUIDv7 to clone." },
                        "new_title": { "type": "string", "description": "Optional title for the cloned independent session." }
                    },
                    "required": ["session_id"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "rename_session",
                "description": "Rename a session by its UUIDv7 identifier.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "session_id": { "type": "string", "description": "Target session UUIDv7." },
                        "new_title": { "type": "string", "description": "New display title for the session." }
                    },
                    "required": ["session_id", "new_title"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "set_session_pinned",
                "description": "Pin or unpin a session in the session list.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "session_id": { "type": "string", "description": "Target session UUIDv7." },
                        "pinned": { "type": "boolean", "description": "True to pin, false to unpin." }
                    },
                    "required": ["session_id", "pinned"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "set_session_archived",
                "description": "Archive or unarchive a session.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "session_id": { "type": "string", "description": "Target session UUIDv7." },
                        "archived": { "type": "boolean", "description": "True to archive, false to restore to active." }
                    },
                    "required": ["session_id", "archived"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "delete_session",
                "description": "Permanently delete a session and remove its isolated filesystem sandbox.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "session_id": { "type": "string", "description": "Target session UUIDv7 to delete." }
                    },
                    "required": ["session_id"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search_sessions",
                "description": "High-speed SIMD + N-Gram indexed content search across one, multiple, or all sessions.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "session_ids": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Optional list of session UUIDv7s to restrict search scope. Omit or pass empty to search all sessions."
                        },
                        "query": { "type": "string", "description": "Keyword or phrase to search for." },
                        "limit": { "type": "integer", "description": "Maximum number of hits to return (default 50)." }
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "inspect_by_id",
                "description": "Look up a specific session or a single message record by its unique UUIDv7 identifier.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "Session UUIDv7 or Message UUIDv7 to inspect." }
                    },
                    "required": ["id"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "set_theme_mode",
                "description": "Change the client theme mode (dark, light, or system).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "mode": { "type": "string", "enum": ["dark", "light", "system"] }
                    },
                    "required": ["mode"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "set_language",
                "description": "Change the client display language (zh or en).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "lang": { "type": "string", "enum": ["zh", "en"] }
                    },
                    "required": ["lang"]
                }
            }
        }
    ])
    .as_array()
    .cloned()
    .unwrap_or_default()
}

/// Standard error type for Fugue operations.
#[derive(Error, Debug)]
pub enum FugueError {
    #[error("CAS lease failure: key '{0}' conflict")]
    CasConflict(String),

    #[error("Invalid task parameter: {0}")]
    InvalidParameter(String),

    #[error("Omni tool violation: attempt to register forbidden tool '{0}'")]
    ForbiddenTool(String),

    #[error("Task execution failed: {0}")]
    ExecutionFailure(String),

    #[error("Storage I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, FugueError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_semantic_feedback_serialization() {
        let feedback = SemanticActionFeedback {
            action_category: ActionCategory::CodeMutation,
            target_subject: "crates/fugue-ui/src/main.rs".to_string(),
            mutation_rationale: "Initialize blank window".to_string(),
            changed_parameters: vec!["window_size".to_string()],
            environmental_side_effects: vec![],
            speculative_next: "Await user interaction".to_string(),
        };
        let serialized = serde_json::to_string(&feedback).expect("Must serialize");
        let deserialized: SemanticActionFeedback =
            serde_json::from_str(&serialized).expect("Must deserialize");
        assert_eq!(feedback, deserialized);
    }

    #[test]
    fn test_uuid_v7_monotonicity_and_roundtrip() {
        let mut prev = UuidV7::new_now();
        for _ in 0..2048 {
            let curr = UuidV7::new_now();
            assert!(
                curr > prev,
                "UUIDv7 must be strictly monotonic: {} <= {}",
                curr,
                prev
            );
            prev = curr;
        }
        let s = prev.to_string();
        assert_eq!(s.len(), 36);
        let parsed = UuidV7::parse_str(&s).expect("Must parse valid UUIDv7");
        assert_eq!(prev, parsed);
        assert!(parsed.timestamp_ms() > 1_700_000_000_000);
    }

    #[test]
    fn test_client_control_tool_schemas_and_parsing() {
        let schemas = client_control_tool_schemas();
        assert_eq!(schemas.len(), 9);

        let cmd = ClientControlCommand::from_tool_call(
            "search_sessions",
            r#"{"query": "Fenwick", "session_ids": ["01930000-0000-7000-8000-000000000001"], "limit": 20}"#,
        )
        .expect("Must parse search_sessions");
        match cmd {
            ClientControlCommand::SearchSessions {
                session_ids,
                query,
                limit,
            } => {
                assert_eq!(query, "Fenwick");
                assert_eq!(session_ids.unwrap().len(), 1);
                assert_eq!(limit, Some(20));
            }
            _ => panic!("Unexpected command variant"),
        }
    }
}
