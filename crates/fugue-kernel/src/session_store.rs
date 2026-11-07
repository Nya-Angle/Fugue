//! # session_store
//! High-performance local multi-session sandbox manager and zero-copy storage engine.
//!
//! ## Core Guarantees
//! 1. **Strict Root Location**: All user-mutable state (sessions, messages, index, theme/language config)
//!    is stored exclusively under `C:\Users\<Username>\.fugue` (`%USERPROFILE%\.fugue`).
//! 2. **Filesystem Sandbox Isolation**: Every session owns an isolated physical directory tree:
//!    `C:\Users\<Username>\.fugue\sessions\<session_uuid_v7>\{storage, workspace, artifacts}`.
//! 3. **Zero-Copy Memory-Mapped Storage (`memmap2` + Relative-Offset Binary Archive)**:
//!    - `messages.idx`: Fixed 32-byte `#[repr(C)]` entries (`msg_id: [u8; 16]`, `data_offset: u64`, `content_len: u32`, `record_len: u32`).
//!    - `messages.bin`: Contiguous UTF-8 message payloads + capsule frames mapped directly into virtual memory.
//!    - `ngram.sig`: 64-byte (512-bit, single CPU Cache-Line aligned) CJK/ASCII N-Gram bitmap signatures.
//! 4. **Two-Tier SIMD Content Indexer**:
//!    - Tier 1: 512-bit Cache-Line bitmap signature filter (`(sig & mask) == mask`) rejects non-matching sessions/messages in < 2ns.
//!    - Tier 2: `memchr::memmem::Finder` (AVX2/SSE2 SIMD) scans zero-copy `mmap` slices directly.
//! 5. **Complete Clone Independence**:
//!    - Cloning a session copies only the mother session's existing content into a brand-new sandbox with new
//!      monotonic `UuidV7` session and message IDs, retaining zero tracking or reference links to the mother session.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use fugue_core::{
    ClientControlCommand, ClientControlResponse, FugueError, InspectEntityResult,
    LanguagePreference, MessageRole, Result, SearchMatchHit, SessionMetadata, StoredActionCapsule,
    StoredMessageRecord, ThemePreference, UuidV7,
};
use memchr::memmem;
use memmap2::Mmap;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

const INDEX_ENTRY_SIZE: usize = 32;
const NGRAM_SIG_BYTES: usize = 64;
const NGRAM_SIG_WORDS: usize = 8;
const CONFIG_MAGIC: &[u8; 8] = b"FUGCFG01";
const REGISTRY_MAGIC: &[u8; 8] = b"FUGREG01";

/// Fixed 32-byte index entry in `storage/messages.idx` (2 entries per 64-byte CPU cache line).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RawMessageIndexEntry {
    pub msg_id: [u8; 16],
    pub data_offset: u64,
    pub content_len: u32,
    pub record_len: u32,
}

impl RawMessageIndexEntry {
    #[inline]
    pub fn to_bytes(&self) -> [u8; INDEX_ENTRY_SIZE] {
        let mut buf = [0u8; INDEX_ENTRY_SIZE];
        buf[0..16].copy_from_slice(&self.msg_id);
        buf[16..24].copy_from_slice(&self.data_offset.to_le_bytes());
        buf[24..28].copy_from_slice(&self.content_len.to_le_bytes());
        buf[28..32].copy_from_slice(&self.record_len.to_le_bytes());
        buf
    }

    #[inline]
    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < INDEX_ENTRY_SIZE {
            return None;
        }
        let mut msg_id = [0u8; 16];
        msg_id.copy_from_slice(&buf[0..16]);
        let data_offset = u64::from_le_bytes(buf[16..24].try_into().ok()?);
        let content_len = u32::from_le_bytes(buf[24..28].try_into().ok()?);
        let record_len = u32::from_le_bytes(buf[28..32].try_into().ok()?);
        Some(Self {
            msg_id,
            data_offset,
            content_len,
            record_len,
        })
    }
}

/// 512-bit (64-byte) Cache-Line aligned N-Gram signature for ultra-fast bitwise pre-filtering.
#[repr(C, align(64))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NgramSignature(pub [u64; NGRAM_SIG_WORDS]);

impl NgramSignature {
    #[inline]
    pub const fn empty() -> Self {
        Self([0u64; NGRAM_SIG_WORDS])
    }

    #[inline]
    pub fn bitor_assign(&mut self, other: &Self) {
        for i in 0..NGRAM_SIG_WORDS {
            self.0[i] |= other.0[i];
        }
    }

    #[inline]
    pub fn contains_mask(&self, mask: &Self) -> bool {
        (self.0[0] & mask.0[0]) == mask.0[0]
            && (self.0[1] & mask.0[1]) == mask.0[1]
            && (self.0[2] & mask.0[2]) == mask.0[2]
            && (self.0[3] & mask.0[3]) == mask.0[3]
            && (self.0[4] & mask.0[4]) == mask.0[4]
            && (self.0[5] & mask.0[5]) == mask.0[5]
            && (self.0[6] & mask.0[6]) == mask.0[6]
            && (self.0[7] & mask.0[7]) == mask.0[7]
    }

    #[inline]
    pub fn to_bytes(&self) -> [u8; NGRAM_SIG_BYTES] {
        let mut out = [0u8; NGRAM_SIG_BYTES];
        for (i, &w) in self.0.iter().enumerate() {
            out[i * 8..(i + 1) * 8].copy_from_slice(&w.to_le_bytes());
        }
        out
    }

    #[inline]
    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < NGRAM_SIG_BYTES {
            return None;
        }
        let mut words = [0u64; NGRAM_SIG_WORDS];
        for (i, word) in words.iter_mut().enumerate() {
            *word = u64::from_le_bytes(buf[i * 8..(i + 1) * 8].try_into().ok()?);
        }
        Some(Self(words))
    }

    #[inline]
    fn set_hash_bit(&mut self, hash: u64) {
        let bit_idx = (hash as usize) & 511;
        self.0[bit_idx >> 6] |= 1u64 << (bit_idx & 63);
    }
}

#[inline]
fn mix64(mut k: u64) -> u64 {
    k ^= k >> 33;
    k = k.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    k ^= k >> 33;
    k = k.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    k ^= k >> 33;
    k
}

/// Computes a 512-bit Cache-Line aligned N-Gram signature over `text`.
/// Guarantees zero false negatives: if `query` is a substring of `text` (case-insensitive ASCII),
/// `compute_ngram_signature(text).contains_mask(&compute_ngram_signature(query))` is strictly `true`.
pub fn compute_ngram_signature(text: &str) -> NgramSignature {
    let mut sig = NgramSignature::empty();
    let mut prev1: Option<u32> = None;
    let mut prev2: Option<u32> = None;

    for ch in text.chars() {
        if ch.is_whitespace() {
            prev1 = None;
            prev2 = None;
            continue;
        }
        let c0 = ch.to_ascii_lowercase() as u32;

        // 1-Gram bit (supports single-character & single-CJK queries)
        let h1 = mix64((c0 as u64) ^ 0x517C_C1B7_2722_0A95);
        sig.set_hash_bit(h1);
        if c0 >= 0x80 {
            // Second independent bit for CJK / Unicode characters
            sig.set_hash_bit(h1.rotate_right(19));
        }

        // 2-Gram bit (adjacent character pair)
        if let Some(c1) = prev1 {
            let pair = ((c1 as u64) << 21) | (c0 as u64);
            let h2 = mix64(pair ^ 0x9E37_79B9_7F4A_7C15);
            sig.set_hash_bit(h2);
            sig.set_hash_bit(h2.rotate_right(23));

            // 3-Gram bit (three consecutive characters)
            if let Some(c2) = prev2 {
                let triple = ((c2 as u64) << 42) | ((c1 as u64) << 21) | (c0 as u64);
                let h3 = mix64(triple ^ 0xBF58_476D_1CE4_E5B9);
                sig.set_hash_bit(h3);
                sig.set_hash_bit(h3.rotate_right(29));
            }
        }

        prev2 = prev1;
        prev1 = Some(c0);
    }

    sig
}

/// Resolves the canonical Fugue user data directory: `C:\Users\<Username>\.fugue`.
pub fn resolve_user_fugue_root() -> PathBuf {
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        let p = PathBuf::from(profile);
        if !p.as_os_str().is_empty() {
            return p.join(".fugue");
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home);
        if !p.as_os_str().is_empty() {
            return p.join(".fugue");
        }
    }
    if let (Some(drive), Some(path)) = (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH"))
    {
        let mut p = PathBuf::from(drive);
        p.push(path);
        return p.join(".fugue");
    }
    PathBuf::from(r"C:\Users\Default\.fugue")
}

/// Persisted client configuration stored in `C:\Users\<Username>\.fugue\config.rkyv`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedClientConfig {
    pub theme: ThemePreference,
    pub lang: LanguagePreference,
    pub sidebar_open: bool,
    pub active_session_id: Option<UuidV7>,
}

impl Default for PersistedClientConfig {
    fn default() -> Self {
        Self {
            theme: ThemePreference::System,
            lang: LanguagePreference::Zh,
            sidebar_open: true,
            active_session_id: None,
        }
    }
}

/// Physical paths inside a single session sandbox (`C:\Users\<Username>\.fugue\sessions\<id>\`).
#[derive(Debug, Clone)]
pub struct SessionSandboxPaths {
    pub root_dir: PathBuf,
    pub storage_dir: PathBuf,
    pub workspace_dir: PathBuf,
    pub artifacts_dir: PathBuf,
    pub messages_bin_path: PathBuf,
    pub messages_idx_path: PathBuf,
    pub ngram_sig_path: PathBuf,
    pub meta_backup_path: PathBuf,
}

impl SessionSandboxPaths {
    pub fn for_session(sessions_root: &Path, session_id: UuidV7) -> Self {
        let root_dir = sessions_root.join(session_id.to_string());
        let storage_dir = root_dir.join("storage");
        let workspace_dir = root_dir.join("workspace");
        let artifacts_dir = root_dir.join("artifacts");
        let messages_bin_path = storage_dir.join("messages.bin");
        let messages_idx_path = storage_dir.join("messages.idx");
        let ngram_sig_path = storage_dir.join("ngram.sig");
        let meta_backup_path = storage_dir.join("meta.json");
        Self {
            root_dir,
            storage_dir,
            workspace_dir,
            artifacts_dir,
            messages_bin_path,
            messages_idx_path,
            ngram_sig_path,
            meta_backup_path,
        }
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.storage_dir)?;
        fs::create_dir_all(&self.workspace_dir)?;
        fs::create_dir_all(&self.artifacts_dir)?;
        Ok(())
    }
}

/// In-memory zero-copy handle for a single session sandbox.
struct SessionHandle {
    meta: SessionMetadata,
    paths: SessionSandboxPaths,
    index_entries: Vec<RawMessageIndexEntry>,
    ngram_sigs: Vec<NgramSignature>,
    aggregate_sig: NgramSignature,
    messages_mmap: Option<Mmap>,
}

impl SessionHandle {
    /// Refreshes the read-only `memmap2::Mmap` over `messages.bin` if non-empty.
    fn refresh_mmap(&mut self) -> Result<()> {
        self.messages_mmap = None;
        if !self.paths.messages_bin_path.exists() {
            return Ok(());
        }
        let file = File::open(&self.paths.messages_bin_path)?;
        let len = file.metadata()?.len();
        if len > 0 {
            // Safety: file is managed exclusively by this session sandbox and we drop mmap before mutating/deleting.
            let mmap = unsafe { Mmap::map(&file)? };
            self.messages_mmap = Some(mmap);
        }
        Ok(())
    }

    /// Zero-copy slice into the UTF-8 message content at `msg_index`.
    #[inline]
    fn get_content_slice(&self, msg_index: usize) -> Option<&[u8]> {
        let idx = self.index_entries.get(msg_index)?;
        let mmap = self.messages_mmap.as_ref()?;
        let start = (idx.data_offset as usize).checked_add(8)?;
        let end = start.checked_add(idx.content_len as usize)?;
        if end <= mmap.len() {
            Some(&mmap[start..end])
        } else {
            None
        }
    }

    /// Decodes a `StoredMessageRecord` at `msg_index` directly from the memory-mapped region.
    fn decode_message_at(&self, msg_index: usize) -> Result<StoredMessageRecord> {
        let idx = self.index_entries.get(msg_index).ok_or_else(|| {
            FugueError::NotFound(format!(
                "Message index {} out of bounds in session {}",
                msg_index, self.meta.id
            ))
        })?;
        let mmap = self.messages_mmap.as_ref().ok_or_else(|| {
            FugueError::NotFound(format!("Empty mmap for session {}", self.meta.id))
        })?;

        let offset = idx.data_offset as usize;
        let rec_end = offset + (idx.record_len as usize);
        if rec_end > mmap.len() || idx.record_len < 8 {
            return Err(FugueError::ExecutionFailure(format!(
                "Corrupted message record frame at index {}",
                msg_index
            )));
        }

        let frame = &mmap[offset..rec_end];
        let role = MessageRole::from_u8(frame[0]);
        let capsule_count = u16::from_le_bytes([frame[2], frame[3]]) as usize;
        let content_len = u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]) as usize;

        if 8 + content_len > frame.len() {
            return Err(FugueError::ExecutionFailure(
                "Corrupted message content length".into(),
            ));
        }

        let content_str = std::str::from_utf8(&frame[8..8 + content_len])
            .unwrap_or_default()
            .to_string();

        let mut cursor = 8 + content_len;
        let mut capsules = Vec::with_capacity(capsule_count);
        for _ in 0..capsule_count {
            if cursor + 9 > frame.len() {
                break;
            }
            let is_expanded = frame[cursor] != 0;
            let sum_len = u32::from_le_bytes([
                frame[cursor + 1],
                frame[cursor + 2],
                frame[cursor + 3],
                frame[cursor + 4],
            ]) as usize;
            let det_len = u32::from_le_bytes([
                frame[cursor + 5],
                frame[cursor + 6],
                frame[cursor + 7],
                frame[cursor + 8],
            ]) as usize;
            cursor += 9;
            if cursor + sum_len + det_len > frame.len() {
                break;
            }
            let summary = std::str::from_utf8(&frame[cursor..cursor + sum_len])
                .unwrap_or_default()
                .to_string();
            cursor += sum_len;
            let detail = std::str::from_utf8(&frame[cursor..cursor + det_len])
                .unwrap_or_default()
                .to_string();
            cursor += det_len;
            capsules.push(StoredActionCapsule {
                summary,
                detail,
                is_expanded,
            });
        }

        let msg_id = UuidV7::from_bytes(idx.msg_id);
        Ok(StoredMessageRecord {
            id: msg_id,
            session_id: self.meta.id,
            role,
            content: content_str,
            created_at_ms: msg_id.timestamp_ms(),
            capsules,
        })
    }
}

/// Encodes a message payload into its binary frame for `storage/messages.bin`.
fn encode_message_frame(
    role: MessageRole,
    content: &str,
    capsules: &[StoredActionCapsule],
) -> Vec<u8> {
    let content_bytes = content.as_bytes();
    let content_len = content_bytes.len() as u32;
    let capsule_count = capsules.len().min(u16::MAX as usize) as u16;

    let extra_cap_bytes: usize = capsules
        .iter()
        .take(capsule_count as usize)
        .map(|c| 9 + c.summary.len() + c.detail.len())
        .sum();

    let mut frame = Vec::with_capacity(8 + content_bytes.len() + extra_cap_bytes);
    frame.push(role as u8);
    frame.push(0u8); // reserved flags
    frame.extend_from_slice(&capsule_count.to_le_bytes());
    frame.extend_from_slice(&content_len.to_le_bytes());
    frame.extend_from_slice(content_bytes);

    for cap in capsules.iter().take(capsule_count as usize) {
        frame.push(u8::from(cap.is_expanded));
        let sum_bytes = cap.summary.as_bytes();
        let det_bytes = cap.detail.as_bytes();
        frame.extend_from_slice(&(sum_bytes.len() as u32).to_le_bytes());
        frame.extend_from_slice(&(det_bytes.len() as u32).to_le_bytes());
        frame.extend_from_slice(sum_bytes);
        frame.extend_from_slice(det_bytes);
    }

    frame
}

/// Extracts a clean, single-line session title from the first user message (up to 24 characters).
pub fn extract_auto_title(content: &str) -> String {
    let first_non_empty = content
        .lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
        .unwrap_or("新会话");

    let mut out = String::new();
    for (i, ch) in first_non_empty.chars().enumerate() {
        if i >= 24 {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    if out.is_empty() {
        "新会话".to_string()
    } else {
        out
    }
}

/// Extracts a short single-line preview snippet (up to 48 characters).
fn extract_preview_snippet(content: &str) -> String {
    let cleaned: String = content
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    let mut out = String::new();
    for (i, ch) in trimmed.chars().enumerate() {
        if i >= 48 {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

struct StoreInner {
    root_dir: PathBuf,
    sessions_dir: PathBuf,
    config_path: PathBuf,
    registry_path: PathBuf,
    config: PersistedClientConfig,
    sessions: HashMap<UuidV7, SessionHandle>,
    /// O(1) global lookup table mapping `MessageId -> (SessionId, message_index)`.
    message_to_session: HashMap<UuidV7, (UuidV7, usize)>,
}

/// Thread-safe, zero-copy multi-session sandbox manager rooted at `C:\Users\<Username>\.fugue`.
pub struct SessionSandboxStore {
    inner: RwLock<StoreInner>,
}

impl SessionSandboxStore {
    /// Opens or initializes the store at the canonical `C:\Users\<Username>\.fugue` directory.
    pub fn open_default() -> Result<Self> {
        Self::open_at(resolve_user_fugue_root())
    }

    /// Opens or initializes the store at a specified root directory (used by unit tests for isolation
    /// while production strictly uses `open_default()`).
    pub fn open_at(root_dir: PathBuf) -> Result<Self> {
        let sessions_dir = root_dir.join("sessions");
        fs::create_dir_all(&sessions_dir)?;

        let config_path = root_dir.join("config.rkyv");
        let registry_path = root_dir.join("sessions_registry.rkyv");

        let config = Self::load_config_file(&config_path).unwrap_or_default();
        let registry_metas = Self::load_registry_file(&registry_path).unwrap_or_default();

        let mut sessions = HashMap::new();
        let mut message_to_session = HashMap::new();

        for mut meta in registry_metas {
            let paths = SessionSandboxPaths::for_session(&sessions_dir, meta.id);
            if !paths.root_dir.exists() {
                continue;
            }
            let _ = paths.ensure_dirs();
            if let Ok(handle) =
                Self::load_session_handle_from_disk(meta.clone(), paths, &mut message_to_session)
            {
                meta.message_count = handle.index_entries.len() as u32;
                sessions.insert(meta.id, handle);
            }
        }

        // Also discover any session sandbox folders on disk not yet in registry
        if let Ok(entries) = fs::read_dir(&sessions_dir) {
            for entry in entries.flatten() {
                if let Ok(ft) = entry.file_type() {
                    if !ft.is_dir() {
                        continue;
                    }
                    let folder_name = entry.file_name().to_string_lossy().to_string();
                    let Ok(sid) = UuidV7::parse_str(&folder_name) else {
                        continue;
                    };
                    if sessions.contains_key(&sid) {
                        continue;
                    }
                    let paths = SessionSandboxPaths::for_session(&sessions_dir, sid);
                    if let Ok(meta_bytes) = fs::read(&paths.meta_backup_path) {
                        if let Ok(meta) = serde_json::from_slice::<SessionMetadata>(&meta_bytes) {
                            if let Ok(handle) = Self::load_session_handle_from_disk(
                                meta,
                                paths,
                                &mut message_to_session,
                            ) {
                                sessions.insert(sid, handle);
                            }
                        }
                    }
                }
            }
        }

        let store = Self {
            inner: RwLock::new(StoreInner {
                root_dir,
                sessions_dir,
                config_path,
                registry_path,
                config,
                sessions,
                message_to_session,
            }),
        };

        // Ensure at least one default session exists on first launch
        {
            let has_any = !store.inner.read().sessions.is_empty();
            if !has_any {
                let default_meta = store.create_session(None)?;
                let mut guard = store.inner.write();
                guard.config.active_session_id = Some(default_meta.id);
                Self::save_config_file(&guard.config_path, &guard.config)?;
            } else {
                let mut guard = store.inner.write();
                let active_valid = guard
                    .config
                    .active_session_id
                    .is_some_and(|id| guard.sessions.contains_key(&id));
                if !active_valid {
                    let first_id = Self::sorted_session_list(&guard.sessions, false)
                        .first()
                        .map(|m| m.id)
                        .or_else(|| {
                            Self::sorted_session_list(&guard.sessions, true)
                                .first()
                                .map(|m| m.id)
                        });
                    guard.config.active_session_id = first_id;
                    let _ = Self::save_config_file(&guard.config_path, &guard.config);
                }
            }
        }

        Ok(store)
    }

    fn load_session_handle_from_disk(
        mut meta: SessionMetadata,
        paths: SessionSandboxPaths,
        message_to_session: &mut HashMap<UuidV7, (UuidV7, usize)>,
    ) -> Result<SessionHandle> {
        let mut index_entries = Vec::new();
        if paths.messages_idx_path.exists() {
            let idx_bytes = fs::read(&paths.messages_idx_path)?;
            for (i, chunk) in idx_bytes.as_chunks::<INDEX_ENTRY_SIZE>().0.iter().enumerate() {
                if let Some(entry) = RawMessageIndexEntry::from_bytes(chunk) {
                    let mid = UuidV7::from_bytes(entry.msg_id);
                    message_to_session.insert(mid, (meta.id, i));
                    index_entries.push(entry);
                }
            }
        }

        let mut ngram_sigs = Vec::with_capacity(index_entries.len());
        let mut aggregate_sig = NgramSignature::empty();
        if paths.ngram_sig_path.exists() {
            let sig_bytes = fs::read(&paths.ngram_sig_path)?;
            for chunk in sig_bytes.as_chunks::<NGRAM_SIG_BYTES>().0 {
                if let Some(sig) = NgramSignature::from_bytes(chunk) {
                    aggregate_sig.bitor_assign(&sig);
                    ngram_sigs.push(sig);
                }
            }
        }

        // Also index session title in aggregate_sig so title matches are never skipped
        let title_sig = compute_ngram_signature(&meta.title);
        aggregate_sig.bitor_assign(&title_sig);

        meta.message_count = index_entries.len() as u32;

        let mut handle = SessionHandle {
            meta,
            paths,
            index_entries,
            ngram_sigs,
            aggregate_sig,
            messages_mmap: None,
        };
        handle.refresh_mmap()?;
        Ok(handle)
    }

    fn load_config_file(path: &Path) -> Option<PersistedClientConfig> {
        let bytes = fs::read(path).ok()?;
        if bytes.len() < 8 || &bytes[0..8] != CONFIG_MAGIC {
            return None;
        }
        serde_json::from_slice(&bytes[8..]).ok()
    }

    fn save_config_file(path: &Path, config: &PersistedClientConfig) -> Result<()> {
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(CONFIG_MAGIC);
        serde_json::to_writer(&mut buf, config)?;
        let tmp_path = path.with_extension("rkyv.tmp");
        fs::write(&tmp_path, &buf)?;
        fs::rename(&tmp_path, path)?;
        Ok(())
    }

    fn load_registry_file(path: &Path) -> Option<Vec<SessionMetadata>> {
        let bytes = fs::read(path).ok()?;
        if bytes.len() < 8 || &bytes[0..8] != REGISTRY_MAGIC {
            return None;
        }
        serde_json::from_slice(&bytes[8..]).ok()
    }

    fn save_registry_locked(inner: &StoreInner) -> Result<()> {
        let metas: Vec<&SessionMetadata> = inner.sessions.values().map(|h| &h.meta).collect();
        let mut buf = Vec::with_capacity(metas.len() * 160 + 16);
        buf.extend_from_slice(REGISTRY_MAGIC);
        serde_json::to_writer(&mut buf, &metas)?;
        let tmp_path = inner.registry_path.with_extension("rkyv.tmp");
        fs::write(&tmp_path, &buf)?;
        fs::rename(&tmp_path, &inner.registry_path)?;
        Ok(())
    }

    fn save_session_meta_backup(handle: &SessionHandle) -> Result<()> {
        let bytes = serde_json::to_vec(&handle.meta)?;
        fs::write(&handle.paths.meta_backup_path, bytes)?;
        Ok(())
    }

    fn sorted_session_list(
        sessions: &HashMap<UuidV7, SessionHandle>,
        include_archived: bool,
    ) -> Vec<SessionMetadata> {
        let mut list: Vec<SessionMetadata> = sessions
            .values()
            .filter(|h| h.meta.archived == include_archived)
            .map(|h| h.meta.clone())
            .collect();

        // Sort order: Pinned first -> Updated timestamp descending -> UUIDv7 descending
        list.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then_with(|| b.updated_at_ms.cmp(&a.updated_at_ms))
                .then_with(|| b.id.cmp(&a.id))
        });
        list
    }

    /// Returns the root `.fugue` directory path (`C:\Users\<Username>\.fugue`).
    pub fn root_dir(&self) -> PathBuf {
        self.inner.read().root_dir.clone()
    }

    /// Returns the current persisted client configuration.
    pub fn get_config(&self) -> PersistedClientConfig {
        self.inner.read().config.clone()
    }

    /// Updates and persists the client configuration (`config.rkyv`).
    pub fn update_config<F>(&self, mutator: F) -> Result<PersistedClientConfig>
    where
        F: FnOnce(&mut PersistedClientConfig),
    {
        let mut guard = self.inner.write();
        mutator(&mut guard.config);
        Self::save_config_file(&guard.config_path, &guard.config)?;
        Ok(guard.config.clone())
    }

    /// Lists active (`archived == false`) or archived (`archived == true`) sessions sorted by
    /// pinned status and recency.
    pub fn list_sessions(&self, archived: bool) -> Vec<SessionMetadata> {
        let guard = self.inner.read();
        Self::sorted_session_list(&guard.sessions, archived)
    }

    /// Lists all sessions (both active and archived).
    pub fn list_all_sessions(&self) -> Vec<SessionMetadata> {
        let guard = self.inner.read();
        let mut list: Vec<SessionMetadata> =
            guard.sessions.values().map(|h| h.meta.clone()).collect();
        list.sort_by(|a, b| {
            a.archived
                .cmp(&b.archived)
                .then_with(|| b.pinned.cmp(&a.pinned))
                .then_with(|| b.updated_at_ms.cmp(&a.updated_at_ms))
                .then_with(|| b.id.cmp(&a.id))
        });
        list
    }

    /// Returns metadata for a specific session in O(1).
    pub fn get_session_metadata(&self, session_id: UuidV7) -> Result<SessionMetadata> {
        let guard = self.inner.read();
        guard
            .sessions
            .get(&session_id)
            .map(|h| h.meta.clone())
            .ok_or_else(|| FugueError::NotFound(format!("Session '{}' not found", session_id)))
    }

    /// Returns the isolated physical sandbox paths (`storage/`, `workspace/`, `artifacts/`) for a session.
    pub fn get_sandbox_paths(&self, session_id: UuidV7) -> Result<SessionSandboxPaths> {
        let guard = self.inner.read();
        guard
            .sessions
            .get(&session_id)
            .map(|h| h.paths.clone())
            .ok_or_else(|| FugueError::NotFound(format!("Session '{}' not found", session_id)))
    }

    /// Creates a brand-new isolated session sandbox.
    pub fn create_session(&self, title: Option<String>) -> Result<SessionMetadata> {
        let mut guard = self.inner.write();
        let id = UuidV7::new_now();
        let now_ms = id.timestamp_ms();
        let custom_titled = title.is_some();
        let default_title = match guard.config.lang {
            LanguagePreference::Zh => "新会话".to_string(),
            LanguagePreference::En => "New Session".to_string(),
        };
        let session_title = title
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .unwrap_or(default_title);

        let paths = SessionSandboxPaths::for_session(&guard.sessions_dir, id);
        paths.ensure_dirs()?;

        let meta = SessionMetadata {
            id,
            title: session_title.clone(),
            pinned: false,
            archived: false,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            message_count: 0,
            preview_snippet: String::new(),
            custom_titled,
        };

        let aggregate_sig = compute_ngram_signature(&session_title);
        let handle = SessionHandle {
            meta: meta.clone(),
            paths,
            index_entries: Vec::new(),
            ngram_sigs: Vec::new(),
            aggregate_sig,
            messages_mmap: None,
        };

        Self::save_session_meta_backup(&handle)?;
        guard.sessions.insert(id, handle);
        guard.config.active_session_id = Some(id);
        Self::save_registry_locked(&guard)?;
        Self::save_config_file(&guard.config_path, &guard.config)?;

        Ok(meta)
    }

    /// Appends a message to a session sandbox in O(1) sequential I/O and updates the mmap view & N-Gram index.
    pub fn append_message(
        &self,
        session_id: UuidV7,
        role: MessageRole,
        content: &str,
        capsules: Vec<StoredActionCapsule>,
    ) -> Result<StoredMessageRecord> {
        let mut guard = self.inner.write();
        let msg_id = UuidV7::new_now();
        let now_ms = msg_id.timestamp_ms();

        let (record, msg_idx) = {
            let handle = guard.sessions.get_mut(&session_id).ok_or_else(|| {
                FugueError::NotFound(format!("Session '{}' not found", session_id))
            })?;

            // Release read mmap before appending to file on Windows
            handle.messages_mmap = None;

            let frame = encode_message_frame(role, content, &capsules);
            let data_offset = if handle.paths.messages_bin_path.exists() {
                fs::metadata(&handle.paths.messages_bin_path)?.len()
            } else {
                0
            };

            let mut bin_file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&handle.paths.messages_bin_path)?;
            bin_file.write_all(&frame)?;
            bin_file.flush()?;
            drop(bin_file);

            let idx_entry = RawMessageIndexEntry {
                msg_id: msg_id.into_bytes(),
                data_offset,
                content_len: content.len() as u32,
                record_len: frame.len() as u32,
            };

            let mut idx_file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&handle.paths.messages_idx_path)?;
            idx_file.write_all(&idx_entry.to_bytes())?;
            idx_file.flush()?;
            drop(idx_file);

            let sig = compute_ngram_signature(content);
            let mut sig_file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&handle.paths.ngram_sig_path)?;
            sig_file.write_all(&sig.to_bytes())?;
            sig_file.flush()?;
            drop(sig_file);

            let msg_idx = handle.index_entries.len();
            handle.index_entries.push(idx_entry);
            handle.ngram_sigs.push(sig);
            handle.aggregate_sig.bitor_assign(&sig);

            // Auto-title on first user message if not custom-renamed
            if role == MessageRole::User && handle.meta.message_count == 0 && !handle.meta.custom_titled
            {
                let auto_title = extract_auto_title(content);
                handle.meta.title = auto_title.clone();
                handle
                    .aggregate_sig
                    .bitor_assign(&compute_ngram_signature(&auto_title));
            }

            handle.meta.message_count = handle.index_entries.len() as u32;
            handle.meta.updated_at_ms = now_ms;
            if !content.trim().is_empty() {
                handle.meta.preview_snippet = extract_preview_snippet(content);
            }

            handle.refresh_mmap()?;
            Self::save_session_meta_backup(handle)?;

            let rec = StoredMessageRecord {
                id: msg_id,
                session_id,
                role,
                content: content.to_string(),
                created_at_ms: now_ms,
                capsules,
            };
            (rec, msg_idx)
        };

        guard
            .message_to_session
            .insert(msg_id, (session_id, msg_idx));
        Self::save_registry_locked(&guard)?;

        Ok(record)
    }

    /// Updates the persisted capsule expansion states for a specific message in a session.
    pub fn update_message_capsules(
        &self,
        session_id: UuidV7,
        msg_index: usize,
        capsules: Vec<StoredActionCapsule>,
    ) -> Result<()> {
        let mut guard = self.inner.write();
        let handle = guard
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| FugueError::NotFound(format!("Session '{}' not found", session_id)))?;

        if msg_index >= handle.index_entries.len() {
            return Ok(());
        }

        let mut all_msgs = Vec::with_capacity(handle.index_entries.len());
        for i in 0..handle.index_entries.len() {
            all_msgs.push(handle.decode_message_at(i)?);
        }
        all_msgs[msg_index].capsules = capsules;

        // Release mmap before rewriting messages.bin and messages.idx
        handle.messages_mmap = None;

        let mut bin_buf = Vec::new();
        let mut idx_buf = Vec::with_capacity(all_msgs.len() * INDEX_ENTRY_SIZE);
        let mut new_entries = Vec::with_capacity(all_msgs.len());

        for msg in &all_msgs {
            let data_offset = bin_buf.len() as u64;
            let frame = encode_message_frame(msg.role, &msg.content, &msg.capsules);
            let record_len = frame.len() as u32;
            bin_buf.extend_from_slice(&frame);

            let entry = RawMessageIndexEntry {
                msg_id: msg.id.into_bytes(),
                data_offset,
                content_len: msg.content.len() as u32,
                record_len,
            };
            idx_buf.extend_from_slice(&entry.to_bytes());
            new_entries.push(entry);
        }

        fs::write(&handle.paths.messages_bin_path, &bin_buf)?;
        fs::write(&handle.paths.messages_idx_path, &idx_buf)?;
        handle.index_entries = new_entries;
        handle.refresh_mmap()?;
        Ok(())
    }

    /// Loads all messages of a session directly from its zero-copy `memmap2` region.
    pub fn load_session_messages(&self, session_id: UuidV7) -> Result<Vec<StoredMessageRecord>> {
        let guard = self.inner.read();
        let handle = guard
            .sessions
            .get(&session_id)
            .ok_or_else(|| FugueError::NotFound(format!("Session '{}' not found", session_id)))?;

        let mut out = Vec::with_capacity(handle.index_entries.len());
        for i in 0..handle.index_entries.len() {
            out.push(handle.decode_message_at(i)?);
        }
        Ok(out)
    }

    /// Clones a mother session into a completely independent child session sandbox.
    ///
    /// - Allocates a brand-new `SessionId (UuidV7)` and creates a separate physical sandbox directory.
    /// - Re-writes all existing messages with brand-new monotonic `MessageId (UuidV7)`s.
    /// - Copies files inside `workspace/` and `artifacts/` to the child sandbox.
    /// - Maintains **zero tracking or reference links** between mother and child sessions.
    pub fn clone_session(
        &self,
        source_session_id: UuidV7,
        new_title: Option<String>,
    ) -> Result<SessionMetadata> {
        // 1. Snapshot mother session data under read lock
        let (mother_title, mother_messages, mother_paths, lang) = {
            let guard = self.inner.read();
            let handle = guard.sessions.get(&source_session_id).ok_or_else(|| {
                FugueError::NotFound(format!("Source session '{}' not found", source_session_id))
            })?;
            let mut msgs = Vec::with_capacity(handle.index_entries.len());
            for i in 0..handle.index_entries.len() {
                msgs.push(handle.decode_message_at(i)?);
            }
            (
                handle.meta.title.clone(),
                msgs,
                handle.paths.clone(),
                guard.config.lang,
            )
        };

        // 2. Build completely independent child session sandbox
        let mut guard = self.inner.write();
        let child_session_id = UuidV7::new_now();
        let now_ms = child_session_id.timestamp_ms();

        let child_title = new_title
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| match lang {
                LanguagePreference::Zh => format!("{} (副本)", mother_title),
                LanguagePreference::En => format!("{} (Copy)", mother_title),
            });

        let child_paths = SessionSandboxPaths::for_session(&guard.sessions_dir, child_session_id);
        child_paths.ensure_dirs()?;

        // Copy isolated workspace/ and artifacts/ physical files into the new sandbox
        let _ = copy_dir_recursive(&mother_paths.workspace_dir, &child_paths.workspace_dir);
        let _ = copy_dir_recursive(&mother_paths.artifacts_dir, &child_paths.artifacts_dir);

        let mut bin_buf = Vec::new();
        let mut idx_buf = Vec::with_capacity(mother_messages.len() * INDEX_ENTRY_SIZE);
        let mut sig_buf = Vec::with_capacity(mother_messages.len() * NGRAM_SIG_BYTES);

        let mut index_entries = Vec::with_capacity(mother_messages.len());
        let mut ngram_sigs = Vec::with_capacity(mother_messages.len());
        let mut aggregate_sig = compute_ngram_signature(&child_title);
        let mut last_preview = String::new();
        let mut new_msg_ids = Vec::with_capacity(mother_messages.len());

        for msg in &mother_messages {
            // Brand-new independent MessageId (UUIDv7) with zero reference to mother message
            let new_mid = UuidV7::new_now();
            let data_offset = bin_buf.len() as u64;
            let frame = encode_message_frame(msg.role, &msg.content, &msg.capsules);
            let record_len = frame.len() as u32;
            bin_buf.extend_from_slice(&frame);

            let idx_entry = RawMessageIndexEntry {
                msg_id: new_mid.into_bytes(),
                data_offset,
                content_len: msg.content.len() as u32,
                record_len,
            };
            idx_buf.extend_from_slice(&idx_entry.to_bytes());
            index_entries.push(idx_entry);

            let sig = compute_ngram_signature(&msg.content);
            sig_buf.extend_from_slice(&sig.to_bytes());
            aggregate_sig.bitor_assign(&sig);
            ngram_sigs.push(sig);

            if !msg.content.trim().is_empty() {
                last_preview = extract_preview_snippet(&msg.content);
            }
            new_msg_ids.push(new_mid);
        }

        if !bin_buf.is_empty() {
            fs::write(&child_paths.messages_bin_path, &bin_buf)?;
            fs::write(&child_paths.messages_idx_path, &idx_buf)?;
            fs::write(&child_paths.ngram_sig_path, &sig_buf)?;
        }

        let child_meta = SessionMetadata {
            id: child_session_id,
            title: child_title,
            pinned: false,
            archived: false,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            message_count: index_entries.len() as u32,
            preview_snippet: last_preview,
            custom_titled: true,
        };

        let mut child_handle = SessionHandle {
            meta: child_meta.clone(),
            paths: child_paths,
            index_entries,
            ngram_sigs,
            aggregate_sig,
            messages_mmap: None,
        };
        child_handle.refresh_mmap()?;
        Self::save_session_meta_backup(&child_handle)?;

        for (i, mid) in new_msg_ids.into_iter().enumerate() {
            guard
                .message_to_session
                .insert(mid, (child_session_id, i));
        }
        guard.sessions.insert(child_session_id, child_handle);
        Self::save_registry_locked(&guard)?;

        Ok(child_meta)
    }

    /// Renames a session and updates its N-Gram signature & persisted metadata.
    pub fn rename_session(&self, session_id: UuidV7, new_title: &str) -> Result<SessionMetadata> {
        let trimmed = new_title.trim();
        if trimmed.is_empty() {
            return Err(FugueError::InvalidParameter(
                "Session title cannot be empty".into(),
            ));
        }

        let mut guard = self.inner.write();
        let meta = {
            let handle = guard.sessions.get_mut(&session_id).ok_or_else(|| {
                FugueError::NotFound(format!("Session '{}' not found", session_id))
            })?;
            handle.meta.title = trimmed.to_string();
            handle.meta.custom_titled = true;
            handle.meta.updated_at_ms = UuidV7::new_now().timestamp_ms();
            handle
                .aggregate_sig
                .bitor_assign(&compute_ngram_signature(trimmed));
            Self::save_session_meta_backup(handle)?;
            handle.meta.clone()
        };
        Self::save_registry_locked(&guard)?;
        Ok(meta)
    }

    /// Pins or unpins a session.
    pub fn set_session_pinned(&self, session_id: UuidV7, pinned: bool) -> Result<SessionMetadata> {
        let mut guard = self.inner.write();
        let meta = {
            let handle = guard.sessions.get_mut(&session_id).ok_or_else(|| {
                FugueError::NotFound(format!("Session '{}' not found", session_id))
            })?;
            handle.meta.pinned = pinned;
            Self::save_session_meta_backup(handle)?;
            handle.meta.clone()
        };
        Self::save_registry_locked(&guard)?;
        Ok(meta)
    }

    /// Archives or unarchives a session.
    pub fn set_session_archived(
        &self,
        session_id: UuidV7,
        archived: bool,
    ) -> Result<SessionMetadata> {
        let mut guard = self.inner.write();
        let meta = {
            let handle = guard.sessions.get_mut(&session_id).ok_or_else(|| {
                FugueError::NotFound(format!("Session '{}' not found", session_id))
            })?;
            handle.meta.archived = archived;
            Self::save_session_meta_backup(handle)?;
            handle.meta.clone()
        };
        Self::save_registry_locked(&guard)?;
        Ok(meta)
    }

    /// Permanently deletes a session and physically removes its sandbox directory from disk.
    pub fn delete_session(&self, session_id: UuidV7) -> Result<()> {
        let sandbox_root = {
            let mut guard = self.inner.write();
            let mut handle = guard.sessions.remove(&session_id).ok_or_else(|| {
                FugueError::NotFound(format!("Session '{}' not found", session_id))
            })?;

            // Explicitly release memory map before filesystem deletion (critical on Windows NTFS)
            handle.messages_mmap = None;
            for entry in &handle.index_entries {
                let mid = UuidV7::from_bytes(entry.msg_id);
                guard.message_to_session.remove(&mid);
            }

            if guard.config.active_session_id == Some(session_id) {
                let next_active = Self::sorted_session_list(&guard.sessions, false)
                    .first()
                    .map(|m| m.id)
                    .or_else(|| {
                        Self::sorted_session_list(&guard.sessions, true)
                            .first()
                            .map(|m| m.id)
                    });
                guard.config.active_session_id = next_active;
                let _ = Self::save_config_file(&guard.config_path, &guard.config);
            }

            Self::save_registry_locked(&guard)?;
            handle.paths.root_dir.clone()
        };

        if sandbox_root.exists() {
            fs::remove_dir_all(&sandbox_root)?;
        }
        Ok(())
    }

    /// High-speed two-tier (512-bit Cache-Line N-Gram Bitmap + AVX2 SIMD `memchr::memmem`) content search
    /// across one, multiple, or all sessions.
    pub fn search_content(
        &self,
        target_sessions: Option<&[UuidV7]>,
        query: &str,
        limit: usize,
    ) -> Vec<SearchMatchHit> {
        let q_trimmed = query.trim();
        if q_trimmed.is_empty() || limit == 0 {
            return Vec::new();
        }

        let query_lower = q_trimmed.to_lowercase();
        let query_mask = compute_ngram_signature(&query_lower);
        let finder = memmem::Finder::new(query_lower.as_bytes());
        let is_pure_ascii_lower = q_trimmed.bytes().all(|b| !b.is_ascii_uppercase());

        let guard = self.inner.read();

        // Collect candidate session IDs
        let candidate_ids: Vec<UuidV7> = match target_sessions {
            Some(ids) if !ids.is_empty() => ids.to_vec(),
            _ => {
                let mut all: Vec<(&UuidV7, u64)> = guard
                    .sessions
                    .iter()
                    .map(|(id, h)| (id, h.meta.updated_at_ms))
                    .collect();
                all.sort_by_key(|a| std::cmp::Reverse(a.1));
                all.into_iter().map(|(id, _)| *id).collect()
            }
        };

        let mut hits = Vec::new();
        let mut lower_scratch = Vec::with_capacity(4096);

        for sid in candidate_ids {
            let Some(handle) = guard.sessions.get(&sid) else {
                continue;
            };

            // Tier 1: O(1) Cache-Line bitwise filter over session aggregate signature
            if !handle.aggregate_sig.contains_mask(&query_mask) {
                continue;
            }

            // Tier 2: Scan message-level 512-bit signatures & zero-copy mmap payload slices
            for (msg_idx, msg_sig) in handle.ngram_sigs.iter().enumerate() {
                if !msg_sig.contains_mask(&query_mask) {
                    continue;
                }
                let Some(raw_slice) = handle.get_content_slice(msg_idx) else {
                    continue;
                };

                // Fast path: direct SIMD scan on raw mmap slice
                let match_pos = if is_pure_ascii_lower {
                    finder.find(raw_slice).or_else(|| {
                        if raw_slice.iter().any(|b| b.is_ascii_uppercase()) {
                            lower_scratch.clear();
                            lower_scratch
                                .extend(raw_slice.iter().map(|b| b.to_ascii_lowercase()));
                            finder.find(&lower_scratch)
                        } else {
                            None
                        }
                    })
                } else {
                    lower_scratch.clear();
                    lower_scratch.extend(raw_slice.iter().map(|b| b.to_ascii_lowercase()));
                    finder.find(&lower_scratch)
                };

                if let Some(byte_offset) = match_pos {
                    let idx_entry = &handle.index_entries[msg_idx];
                    let msg_id = UuidV7::from_bytes(idx_entry.msg_id);
                    let content_str = std::str::from_utf8(raw_slice).unwrap_or_default();
                    let snippet = build_match_snippet(content_str, byte_offset, q_trimmed.len());
                    let role = handle
                        .messages_mmap
                        .as_ref()
                        .and_then(|m| m.get(idx_entry.data_offset as usize))
                        .map(|&b| MessageRole::from_u8(b))
                        .unwrap_or(MessageRole::User);

                    hits.push(SearchMatchHit {
                        session_id: sid,
                        session_title: handle.meta.title.clone(),
                        message_id: msg_id,
                        message_index: msg_idx,
                        role,
                        created_at_ms: msg_id.timestamp_ms(),
                        matched_snippet: snippet,
                        byte_offset_in_msg: byte_offset,
                    });

                    if hits.len() >= limit {
                        return hits;
                    }
                }
            }
        }

        hits
    }

    /// Looks up a specific Session or Message by its globally unique `UuidV7` identifier in O(1).
    pub fn inspect_by_id(&self, id: UuidV7) -> Result<InspectEntityResult> {
        let guard = self.inner.read();

        // 1. Check if `id` is a SessionId
        if let Some(handle) = guard.sessions.get(&id) {
            let mut messages = Vec::with_capacity(handle.index_entries.len());
            for i in 0..handle.index_entries.len() {
                messages.push(handle.decode_message_at(i)?);
            }
            return Ok(InspectEntityResult::Session {
                metadata: handle.meta.clone(),
                sandbox_path: handle.paths.root_dir.to_string_lossy().to_string(),
                messages,
            });
        }

        // 2. Check if `id` is a MessageId via O(1) global index
        if let Some(&(session_id, msg_idx)) = guard.message_to_session.get(&id) {
            if let Some(handle) = guard.sessions.get(&session_id) {
                let message = handle.decode_message_at(msg_idx)?;
                return Ok(InspectEntityResult::Message {
                    session_metadata: handle.meta.clone(),
                    message_index: msg_idx,
                    message,
                });
            }
        }

        Err(FugueError::NotFound(format!(
            "No session or message found with ID '{}'",
            id
        )))
    }

    /// Executes any of the 9 reserved `ClientControlCommand` operations directly against the store.
    pub fn execute_control_command(
        &self,
        cmd: ClientControlCommand,
    ) -> Result<ClientControlResponse> {
        match cmd {
            ClientControlCommand::CloneSession {
                session_id,
                new_title,
            } => {
                let sid = UuidV7::parse_str(&session_id)?;
                let session = self.clone_session(sid, new_title)?;
                Ok(ClientControlResponse::SessionCreatedOrUpdated { session })
            }
            ClientControlCommand::RenameSession {
                session_id,
                new_title,
            } => {
                let sid = UuidV7::parse_str(&session_id)?;
                let session = self.rename_session(sid, &new_title)?;
                Ok(ClientControlResponse::SessionCreatedOrUpdated { session })
            }
            ClientControlCommand::SetSessionPinned { session_id, pinned } => {
                let sid = UuidV7::parse_str(&session_id)?;
                let session = self.set_session_pinned(sid, pinned)?;
                Ok(ClientControlResponse::SessionCreatedOrUpdated { session })
            }
            ClientControlCommand::SetSessionArchived {
                session_id,
                archived,
            } => {
                let sid = UuidV7::parse_str(&session_id)?;
                let session = self.set_session_archived(sid, archived)?;
                Ok(ClientControlResponse::SessionCreatedOrUpdated { session })
            }
            ClientControlCommand::DeleteSession { session_id } => {
                let sid = UuidV7::parse_str(&session_id)?;
                self.delete_session(sid)?;
                Ok(ClientControlResponse::SessionDeleted {
                    deleted_session_id: sid,
                })
            }
            ClientControlCommand::SearchSessions {
                session_ids,
                query,
                limit,
            } => {
                let parsed_ids: Option<Vec<UuidV7>> = match session_ids {
                    Some(list) if !list.is_empty() => {
                        let mut v = Vec::with_capacity(list.len());
                        for s in list {
                            v.push(UuidV7::parse_str(&s)?);
                        }
                        Some(v)
                    }
                    _ => None,
                };
                let hits = self.search_content(
                    parsed_ids.as_deref(),
                    &query,
                    limit.unwrap_or(50).clamp(1, 500),
                );
                Ok(ClientControlResponse::SearchResults { query, hits })
            }
            ClientControlCommand::InspectById { id } => {
                let target_uuid = UuidV7::parse_str(&id)?;
                let result = self.inspect_by_id(target_uuid)?;
                Ok(ClientControlResponse::Inspected {
                    result: Box::new(result),
                })
            }
            ClientControlCommand::SetThemeMode { mode } => {
                let cfg = self.update_config(|c| c.theme = mode)?;
                Ok(ClientControlResponse::SettingsUpdated {
                    theme: cfg.theme,
                    lang: cfg.lang,
                })
            }
            ClientControlCommand::SetLanguage { lang } => {
                let cfg = self.update_config(|c| c.lang = lang)?;
                Ok(ClientControlResponse::SettingsUpdated {
                    theme: cfg.theme,
                    lang: cfg.lang,
                })
            }
        }
    }
}

/// Builds a clean UTF-8 context snippet centered around `byte_offset`.
fn build_match_snippet(content: &str, byte_offset: usize, match_len: usize) -> String {
    let mut start = byte_offset.saturating_sub(28);
    while start > 0 && !content.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (byte_offset + match_len + 36).min(content.len());
    while end < content.len() && !content.is_char_boundary(end) {
        end += 1;
    }

    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    for ch in content[start..end].chars() {
        if ch == '\n' || ch == '\r' {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    if end < content.len() {
        out.push('…');
    }
    out
}

/// Recursively copies files and subdirectories from `src` to `dst`.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    if !src.exists() {
        return Ok(());
    }
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &dst_path)?;
        } else if ty.is_file() {
            fs::copy(entry.path(), dst_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_temp_test_dir(tag: &str) -> PathBuf {
        let id = UuidV7::new_now();
        let dir = std::env::temp_dir().join(format!("fugue_test_{}_{}", tag, id));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn test_user_fugue_root_points_to_dot_fugue() {
        let root = resolve_user_fugue_root();
        assert_eq!(
            root.file_name().and_then(|n| n.to_str()),
            Some(".fugue"),
            "User root must end with .fugue: {:?}",
            root
        );
    }

    #[test]
    fn test_sandbox_isolation_zero_copy_read_write_and_clone_independence() {
        let test_root = create_temp_test_dir("clone_indep");
        let store = SessionSandboxStore::open_at(test_root.clone()).expect("Must open store");

        // 1. Default session created automatically
        let initial_sessions = store.list_sessions(false);
        assert_eq!(initial_sessions.len(), 1);
        let mother_id = initial_sessions[0].id;

        // Verify physical sandbox subdirectories exist
        let mother_paths = store
            .get_sandbox_paths(mother_id)
            .expect("Must get mother paths");
        assert!(mother_paths.storage_dir.exists());
        assert!(mother_paths.workspace_dir.exists());
        assert!(mother_paths.artifacts_dir.exists());

        // Write a sandbox file into mother's workspace
        fs::write(mother_paths.workspace_dir.join("main.rs"), "fn main() {}").unwrap();

        // 2. Append user & assistant messages
        let m1 = store
            .append_message(
                mother_id,
                MessageRole::User,
                "请帮我实现 Fenwick 树视口裁剪算法",
                vec![],
            )
            .unwrap();
        let m2 = store
            .append_message(
                mother_id,
                MessageRole::Assistant,
                "好的，Fenwick 树支持 O(log N) 前缀和更新与树上倍增二分查找。",
                vec![StoredActionCapsule {
                    summary: "派发任务: new_task".into(),
                    detail: "sandbox execution".into(),
                    is_expanded: false,
                }],
            )
            .unwrap();

        // Auto-title should have extracted from m1
        let mother_meta = store.get_session_metadata(mother_id).unwrap();
        assert!(mother_meta.title.contains("Fenwick"));

        // 3. Clone mother session into child session
        let child_meta = store
            .clone_session(mother_id, None)
            .expect("Must clone session");
        let child_id = child_meta.id;
        assert_ne!(mother_id, child_id);

        let child_msgs = store.load_session_messages(child_id).unwrap();
        assert_eq!(child_msgs.len(), 2);
        assert_eq!(child_msgs[0].content, m1.content);
        assert_eq!(child_msgs[1].content, m2.content);
        assert_eq!(child_msgs[1].capsules.len(), 1);
        // Crucial requirement: Child messages must have brand-new UUIDv7 IDs
        assert_ne!(child_msgs[0].id, m1.id);
        assert_ne!(child_msgs[1].id, m2.id);

        // Child workspace file was copied independently
        let child_paths = store.get_sandbox_paths(child_id).unwrap();
        assert!(child_paths.workspace_dir.join("main.rs").exists());

        // 4. Mutate and delete mother session -> Child session must remain 100% intact and unaffected
        store
            .append_message(mother_id, MessageRole::User, "母会话后续新消息", vec![])
            .unwrap();
        store
            .delete_session(mother_id)
            .expect("Must delete mother session");
        assert!(!mother_paths.root_dir.exists());

        let child_msgs_after = store.load_session_messages(child_id).unwrap();
        assert_eq!(child_msgs_after.len(), 2);
        assert!(child_paths.root_dir.exists());

        let _ = fs::remove_dir_all(&test_root);
    }

    #[test]
    fn test_simd_ngram_search_and_inspect_by_id() {
        let test_root = create_temp_test_dir("simd_search");
        let store = SessionSandboxStore::open_at(test_root.clone()).expect("Must open store");

        let s1 = store.list_sessions(false)[0].id;
        let s2 = store
            .create_session(Some("Rust 零拷贝架构".into()))
            .unwrap()
            .id;

        let msg_s1 = store
            .append_message(
                s1,
                MessageRole::User,
                "探讨 Lenis duration=1.2 lerp=0.1 平滑滚动物理公式",
                vec![],
            )
            .unwrap();
        let msg_s2 = store
            .append_message(
                s2,
                MessageRole::Assistant,
                "采用 memmap2 与 UUIDv7 实现微秒级零拷贝消息索引",
                vec![],
            )
            .unwrap();

        // Global search (all sessions)
        let hits_all = store.search_content(None, "UUIDv7", 10);
        assert_eq!(hits_all.len(), 1);
        assert_eq!(hits_all[0].session_id, s2);
        assert_eq!(hits_all[0].message_id, msg_s2.id);

        // Scoped search (only s1 -> should not match UUIDv7, should match Lenis)
        let hits_s1_miss = store.search_content(Some(&[s1]), "UUIDv7", 10);
        assert!(hits_s1_miss.is_empty());

        let hits_s1_hit = store.search_content(Some(&[s1]), "平滑滚动", 10);
        assert_eq!(hits_s1_hit.len(), 1);
        assert_eq!(hits_s1_hit[0].message_id, msg_s1.id);

        // Inspect by SessionId and by MessageId via ClientControlCommand
        let resp_msg = store
            .execute_control_command(ClientControlCommand::InspectById {
                id: msg_s2.id.to_string(),
            })
            .unwrap();
        match resp_msg {
            ClientControlResponse::Inspected { result } => match *result {
                InspectEntityResult::Message {
                    session_metadata,
                    message_index,
                    message,
                } => {
                    assert_eq!(session_metadata.id, s2);
                    assert_eq!(message_index, 0);
                    assert_eq!(message.id, msg_s2.id);
                }
                _ => panic!("Expected Message inspect result"),
            },
            _ => panic!("Expected Inspected response"),
        }

        let _ = fs::remove_dir_all(&test_root);
    }
}
