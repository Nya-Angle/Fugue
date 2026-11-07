//! # client_control
//! Client Control Gateway for both UI interactions and future LLM-driven client operations.
//!
//! Exposes a unified, strongly-typed Rust API and JSON Function-Calling router for all 9
//! required client operations while strictly preserving AXIOM[03] (`new_task` tool purity on Omni):
//! 1. **Clone Session** (`clone_session`) — independent sandbox & new UUIDv7s with zero tracking links
//! 2. **Rename Session** (`rename_session`)
//! 3. **Pin / Unpin Session** (`set_session_pinned`)
//! 4. **Archive / Unarchive Session** (`set_session_archived`)
//! 5. **Delete Session** (`delete_session`)
//! 6. **Index / Search Content across One, Multiple, or All Sessions** (`search_sessions`)
//! 7. **Inspect Session or Message Record by Unique ID** (`inspect_by_id`)
//! 8. **Change Dark / Light / System Theme Mode** (`set_theme_mode`)
//! 9. **Change Client Language** (`set_language`)

use std::sync::Arc;

use fugue_core::{
    client_control_tool_schemas, ClientControlCommand, ClientControlResponse, InspectEntityResult,
    LanguagePreference, Result, SearchMatchHit, SessionMetadata, ThemePreference, UuidV7,
};
use fugue_kernel::SessionSandboxStore;

/// Callback invoked whenever a `ClientControlCommand` mutates client state.
pub type UiSyncHook = Arc<dyn Fn(&ClientControlResponse) + Send + Sync>;

/// Thread-safe Client Control Gateway backed by `SessionSandboxStore` (`C:\Users\<Username>\.fugue`).
#[derive(Clone)]
pub struct ClientControlGateway {
    store: Arc<SessionSandboxStore>,
    ui_sync_hook: Option<UiSyncHook>,
}

impl ClientControlGateway {
    pub fn new(
        store: Arc<SessionSandboxStore>,
        ui_sync_hook: Option<UiSyncHook>,
    ) -> Self {
        Self {
            store,
            ui_sync_hook,
        }
    }

    #[inline]
    pub fn store(&self) -> &Arc<SessionSandboxStore> {
        &self.store
    }

    /// Returns the OpenAI/DeepSeek Function-Calling JSON schemas for all 9 reserved client operations.
    #[inline]
    pub fn tool_schemas() -> Vec<serde_json::Value> {
        client_control_tool_schemas()
    }

    /// Executes a strongly-typed `ClientControlCommand`, persists changes to `C:\Users\<Username>\.fugue`,
    /// and triggers the UI synchronization hook if registered.
    pub fn execute_command(&self, cmd: ClientControlCommand) -> Result<ClientControlResponse> {
        let resp = self.store.execute_control_command(cmd)?;
        if let Some(ref hook) = self.ui_sync_hook {
            hook(&resp);
        }
        Ok(resp)
    }

    /// Executes an LLM tool call (`tool_name`, `arguments_json`) and returns a serialized JSON response.
    pub fn execute_tool_call_json(&self, tool_name: &str, arguments_json: &str) -> Result<String> {
        let cmd = ClientControlCommand::from_tool_call(tool_name, arguments_json)?;
        let resp = self.execute_command(cmd)?;
        Ok(serde_json::to_string(&resp)?)
    }

    // =========================================================================
    // Direct Ergonomic Wrappers for the 9 Reserved Operations
    // =========================================================================

    /// 1. Clone a session into a completely independent new session sandbox.
    pub fn clone_session(
        &self,
        session_id: &str,
        new_title: Option<String>,
    ) -> Result<SessionMetadata> {
        match self.execute_command(ClientControlCommand::CloneSession {
            session_id: session_id.to_string(),
            new_title,
        })? {
            ClientControlResponse::SessionCreatedOrUpdated { session } => Ok(session),
            _ => unreachable!(),
        }
    }

    /// 2. Rename a session.
    pub fn rename_session(&self, session_id: &str, new_title: &str) -> Result<SessionMetadata> {
        match self.execute_command(ClientControlCommand::RenameSession {
            session_id: session_id.to_string(),
            new_title: new_title.to_string(),
        })? {
            ClientControlResponse::SessionCreatedOrUpdated { session } => Ok(session),
            _ => unreachable!(),
        }
    }

    /// 3. Pin or unpin a session.
    pub fn set_session_pinned(&self, session_id: &str, pinned: bool) -> Result<SessionMetadata> {
        match self.execute_command(ClientControlCommand::SetSessionPinned {
            session_id: session_id.to_string(),
            pinned,
        })? {
            ClientControlResponse::SessionCreatedOrUpdated { session } => Ok(session),
            _ => unreachable!(),
        }
    }

    /// 4. Archive or unarchive a session.
    pub fn set_session_archived(
        &self,
        session_id: &str,
        archived: bool,
    ) -> Result<SessionMetadata> {
        match self.execute_command(ClientControlCommand::SetSessionArchived {
            session_id: session_id.to_string(),
            archived,
        })? {
            ClientControlResponse::SessionCreatedOrUpdated { session } => Ok(session),
            _ => unreachable!(),
        }
    }

    /// 5. Permanently delete a session and its isolated filesystem sandbox.
    pub fn delete_session(&self, session_id: &str) -> Result<UuidV7> {
        match self.execute_command(ClientControlCommand::DeleteSession {
            session_id: session_id.to_string(),
        })? {
            ClientControlResponse::SessionDeleted { deleted_session_id } => Ok(deleted_session_id),
            _ => unreachable!(),
        }
    }

    /// 6. Search specific content across one, multiple, or all sessions via N-Gram Bitmap + AVX2 SIMD.
    pub fn search_sessions(
        &self,
        session_ids: Option<Vec<String>>,
        query: &str,
        limit: Option<usize>,
    ) -> Result<Vec<SearchMatchHit>> {
        match self.execute_command(ClientControlCommand::SearchSessions {
            session_ids,
            query: query.to_string(),
            limit,
        })? {
            ClientControlResponse::SearchResults { hits, .. } => Ok(hits),
            _ => unreachable!(),
        }
    }

    /// 7. Inspect a specific session or message record by its globally unique UUIDv7 ID.
    pub fn inspect_by_id(&self, id: &str) -> Result<InspectEntityResult> {
        match self.execute_command(ClientControlCommand::InspectById { id: id.to_string() })? {
            ClientControlResponse::Inspected { result } => Ok(*result),
            _ => unreachable!(),
        }
    }

    /// 8. Change client theme mode (`Dark`, `Light`, `System`).
    pub fn set_theme_mode(
        &self,
        mode: ThemePreference,
    ) -> Result<(ThemePreference, LanguagePreference)> {
        match self.execute_command(ClientControlCommand::SetThemeMode { mode })? {
            ClientControlResponse::SettingsUpdated { theme, lang } => Ok((theme, lang)),
            _ => unreachable!(),
        }
    }

    /// 9. Change client display language (`Zh`, `En`).
    pub fn set_language(
        &self,
        lang: LanguagePreference,
    ) -> Result<(ThemePreference, LanguagePreference)> {
        match self.execute_command(ClientControlCommand::SetLanguage { lang })? {
            ClientControlResponse::SettingsUpdated { theme, lang } => Ok((theme, lang)),
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_control_gateway_all_9_operations() {
        let temp_root = std::env::temp_dir().join(format!("fugue_gw_test_{}", UuidV7::new_now()));
        let store = Arc::new(SessionSandboxStore::open_at(temp_root.clone()).unwrap());
        let gateway = ClientControlGateway::new(store.clone(), None);

        let initial_id = store.list_sessions(false)[0].id.to_string();
        store
            .append_message(
                UuidV7::parse_str(&initial_id).unwrap(),
                fugue_core::MessageRole::User,
                "极致性能 Rust 存储测试",
                vec![],
            )
            .unwrap();

        // 1. Rename via JSON tool call
        let rename_json = gateway
            .execute_tool_call_json(
                "rename_session",
                &format!(
                    r#"{{"session_id": "{}", "new_title": "架构核心讨论"}}"#,
                    initial_id
                ),
            )
            .unwrap();
        assert!(rename_json.contains("架构核心讨论"));

        // 2. Pin via JSON tool call
        gateway
            .execute_tool_call_json(
                "set_session_pinned",
                &format!(r#"{{"session_id": "{}", "pinned": true}}"#, initial_id),
            )
            .unwrap();
        assert!(
            store
                .get_session_metadata(UuidV7::parse_str(&initial_id).unwrap())
                .unwrap()
                .pinned
        );

        // 3. Clone via JSON tool call
        let clone_resp = gateway
            .clone_session(&initial_id, Some("独立克隆会话".into()))
            .unwrap();
        assert_ne!(clone_resp.id.to_string(), initial_id);
        assert_eq!(clone_resp.title, "独立克隆会话");

        // 4. Search across specific sessions
        let hits = gateway
            .search_sessions(Some(vec![clone_resp.id.to_string()]), "极致性能", Some(10))
            .unwrap();
        assert_eq!(hits.len(), 1);

        // 5. Inspect by message ID
        let inspected = gateway
            .inspect_by_id(&hits[0].message_id.to_string())
            .unwrap();
        match inspected {
            InspectEntityResult::Message { message, .. } => {
                assert!(message.content.contains("极致性能"));
            }
            _ => panic!("Expected message"),
        }

        // 6. Archive & Delete
        gateway
            .set_session_archived(&clone_resp.id.to_string(), true)
            .unwrap();
        assert_eq!(store.list_sessions(true).len(), 1);
        gateway.delete_session(&clone_resp.id.to_string()).unwrap();
        assert_eq!(store.list_sessions(true).len(), 0);

        // 7. Theme & Language
        let (theme, _lang) = gateway.set_theme_mode(ThemePreference::Dark).unwrap();
        assert_eq!(theme, ThemePreference::Dark);
        let (_, lang2) = gateway.set_language(LanguagePreference::En).unwrap();
        assert_eq!(lang2, LanguagePreference::En);

        let _ = std::fs::remove_dir_all(&temp_root);
    }
}
