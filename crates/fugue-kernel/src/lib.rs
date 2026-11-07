//! # fugue-kernel
//! Living Node Memory state machine, CAS lease lock, reactive event broadcaster,
//! and zero-copy multi-session sandbox storage engine (`session_store`).

pub mod session_store;
pub use session_store::{
    compute_ngram_signature, extract_auto_title, resolve_user_fugue_root, NgramSignature,
    PersistedClientConfig, RawMessageIndexEntry, SessionSandboxPaths, SessionSandboxStore,
};

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use fugue_core::{FugueError, Result, SemanticActionFeedback, TaskStatus};
use parking_lot::RwLock;
use tokio::sync::broadcast;

/// Event emitted by LivingNodeMemory for peer avatar reactive wakeups.
#[derive(Debug, Clone)]
pub enum MemoryEvent {
    TaskCreated { task_id: String },
    TaskStatusChanged { task_id: String, status: TaskStatus },
    TelemetryAppended { task_id: String, feedback: SemanticActionFeedback },
}

/// A leased task record in the TaskBoard.
#[derive(Debug, Clone)]
pub struct LeasedTask {
    pub id: String,
    pub status: TaskStatus,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<Instant>,
    pub version: u64,
}

/// Living Node Memory Engine conforming to SECTION 3 of the Protocol.
#[derive(Debug)]
pub struct LivingNodeMemory {
    /// IntentStore: Immutable global assertions & constraints
    pub intent_store: RwLock<HashMap<String, String>>,
    /// TaskBoard: Atomic state machine with CAS lease locking
    pub task_board: RwLock<HashMap<String, LeasedTask>>,
    /// SymbolRegistry: Versioned symbol index
    pub symbol_registry: RwLock<HashMap<String, (String, u64)>>,
    /// TelemetryLog: Monotonic list of sanitized semantic feedback
    pub telemetry_log: RwLock<Vec<(String, SemanticActionFeedback, chrono::DateTime<Utc>)>>,
    /// Global version counter for CAS verification
    version_counter: AtomicU64,
    /// Reactive event broadcaster (zero polling)
    event_tx: broadcast::Sender<MemoryEvent>,
}

impl Default for LivingNodeMemory {
    fn default() -> Self {
        Self::new()
    }
}

impl LivingNodeMemory {
    pub fn new() -> Self {
        let (event_tx, _) = broadcast::channel(1024);
        Self {
            intent_store: RwLock::new(HashMap::new()),
            task_board: RwLock::new(HashMap::new()),
            symbol_registry: RwLock::new(HashMap::new()),
            telemetry_log: RwLock::new(Vec::new()),
            version_counter: AtomicU64::new(1),
            event_tx,
        }
    }

    /// Subscribe to memory reactive events.
    pub fn subscribe(&self) -> broadcast::Receiver<MemoryEvent> {
        self.event_tx.subscribe()
    }

    /// Add a new task to the TaskBoard.
    pub fn add_task(&self, task_id: String) {
        let version = self.version_counter.fetch_add(1, Ordering::SeqCst);
        let task = LeasedTask {
            id: task_id.clone(),
            status: TaskStatus::Unclaimed,
            lease_owner: None,
            lease_expires_at: None,
            version,
        };
        self.task_board.write().insert(task_id.clone(), task);
        let _ = self.event_tx.send(MemoryEvent::TaskCreated { task_id });
    }

    /// Attempt to acquire an optimistic CAS lease lock on a task (T_Lease duration).
    pub fn acquire_lease(&self, task_id: &str, peer_id: &str, lease_duration: Duration) -> Result<u64> {
        let mut board = self.task_board.write();
        let task = board.get_mut(task_id).ok_or_else(|| {
            FugueError::InvalidParameter(format!("Task '{}' not found", task_id))
        })?;

        let now = Instant::now();
        let can_lease = match task.status {
            TaskStatus::Unclaimed => true,
            TaskStatus::Leased | TaskStatus::Running => {
                // Check if previous lease has expired
                task.lease_expires_at.is_none_or(|expiry| now >= expiry)
            }
            _ => false,
        };

        if !can_lease {
            return Err(FugueError::CasConflict(task_id.to_string()));
        }

        task.status = TaskStatus::Leased;
        task.lease_owner = Some(peer_id.to_string());
        task.lease_expires_at = Some(now + lease_duration);
        task.version = self.version_counter.fetch_add(1, Ordering::SeqCst);

        let new_version = task.version;
        let _ = self.event_tx.send(MemoryEvent::TaskStatusChanged {
            task_id: task_id.to_string(),
            status: TaskStatus::Leased,
        });

        Ok(new_version)
    }

    /// Append sanitized semantic feedback to the TelemetryLog.
    pub fn append_telemetry(&self, task_id: String, feedback: SemanticActionFeedback) {
        let timestamp = Utc::now();
        self.telemetry_log.write().push((task_id.clone(), feedback.clone(), timestamp));
        let _ = self.event_tx.send(MemoryEvent::TelemetryAppended { task_id, feedback });
    }
}

pub type SharedMemory = Arc<LivingNodeMemory>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cas_lease_locking_success_and_conflict() {
        let memory = LivingNodeMemory::new();
        let task_id = "task-001".to_string();
        memory.add_task(task_id.clone());

        // Peer 1 acquires lease
        let version1 = memory
            .acquire_lease(&task_id, "peer-1", Duration::from_secs(30))
            .expect("First lease should succeed");
        assert!(version1 > 0);

        // Peer 2 attempts to lease while active -> Should fail with CasConflict
        let err = memory
            .acquire_lease(&task_id, "peer-2", Duration::from_secs(30))
            .expect_err("Concurrent lease should fail");
        match err {
            FugueError::CasConflict(id) => assert_eq!(id, task_id),
            _ => panic!("Expected CasConflict"),
        }
    }

    #[tokio::test]
    async fn test_telemetry_event_broadcasting() {
        let memory = LivingNodeMemory::new();
        let mut rx = memory.subscribe();

        let feedback = fugue_core::SemanticActionFeedback {
            action_category: fugue_core::ActionCategory::ContentGeneration,
            target_subject: "test".to_string(),
            mutation_rationale: "verify".to_string(),
            changed_parameters: vec![],
            environmental_side_effects: vec![],
            speculative_next: "done".to_string(),
        };

        memory.append_telemetry("task-002".to_string(), feedback);

        let event = rx.recv().await.expect("Must receive event");
        match event {
            MemoryEvent::TelemetryAppended { task_id, .. } => {
                assert_eq!(task_id, "task-002");
            }
            _ => panic!("Expected TelemetryAppended event"),
        }
    }
}

