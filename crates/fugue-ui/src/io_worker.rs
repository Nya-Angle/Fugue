//! # io_worker
//! Single-consumer Tokio async I/O worker for `SessionSandboxStore` operations,
//! keeping all disk reads/writes and full-store searches off the Slint UI thread.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use fugue_core::{
    LanguagePreference, SearchMatchHit, StoredActionCapsule, ThemePreference, UuidV7,
};
use fugue_kernel::SessionSandboxStore;
use tokio::sync::mpsc;

pub enum IoCommand {
    SearchSessions {
        gen_id: u64,
        query: String,
    },
    UpdateThemeConfig {
        theme: ThemePreference,
    },
    UpdateLanguageConfig {
        lang: LanguagePreference,
    },
    UpdateMessageCapsules {
        session_id: UuidV7,
        message_index: usize,
        capsules: Vec<StoredActionCapsule>,
    },
}

#[derive(Clone)]
pub struct AsyncIoHandle {
    tx: mpsc::UnboundedSender<IoCommand>,
    search_gen: Arc<AtomicU64>,
}

impl AsyncIoHandle {
    pub fn spawn<F>(
        rt_handle: &tokio::runtime::Handle,
        store: Arc<SessionSandboxStore>,
        on_search_ready: F,
    ) -> Self
    where
        F: Fn(u64, String, Vec<SearchMatchHit>) + Send + Sync + 'static,
    {
        let (tx, mut rx) = mpsc::unbounded_channel::<IoCommand>();
        let search_gen = Arc::new(AtomicU64::new(0));
        let search_gen_worker = search_gen.clone();
        let on_search_ready = Arc::new(on_search_ready);

        rt_handle.spawn(async move {
            let mut pending_search: Option<(u64, String)> = None;
            let mut search_deadline: Option<tokio::time::Instant> = None;

            loop {
                tokio::select! {
                    biased;
                    _ = async {
                        if let Some(dl) = search_deadline {
                            tokio::time::sleep_until(dl).await;
                        } else {
                            std::future::pending::<()>().await;
                        }
                    }, if search_deadline.is_some() => {
                        search_deadline = None;
                        if let Some((gen_id, query)) = pending_search.take() {
                            if search_gen_worker.load(Ordering::Acquire) == gen_id {
                                let store_c = store.clone();
                                let q_c = query.clone();
                                let hits = tokio::task::spawn_blocking(move || {
                                    store_c.search_content(None, &q_c, 60)
                                })
                                .await
                                .unwrap_or_default();
                                if search_gen_worker.load(Ordering::Acquire) == gen_id {
                                    on_search_ready(gen_id, query, hits);
                                }
                            }
                        }
                    }
                    cmd_opt = rx.recv() => {
                        let Some(cmd) = cmd_opt else {
                            break;
                        };
                        match cmd {
                            IoCommand::SearchSessions { gen_id, query } => {
                                if query.trim().is_empty() {
                                    pending_search = None;
                                    search_deadline = None;
                                    if search_gen_worker.load(Ordering::Acquire) == gen_id {
                                        on_search_ready(gen_id, query, Vec::new());
                                    }
                                } else {
                                    pending_search = Some((gen_id, query));
                                    search_deadline = Some(
                                        tokio::time::Instant::now() + Duration::from_millis(60),
                                    );
                                }
                            }
                            IoCommand::UpdateThemeConfig { theme } => {
                                let _ = store.update_config(|c| c.theme = theme);
                            }
                            IoCommand::UpdateLanguageConfig { lang } => {
                                let _ = store.update_config(|c| c.lang = lang);
                            }
                            IoCommand::UpdateMessageCapsules { session_id, message_index, capsules } => {
                                let _ = store.update_message_capsules(session_id, message_index, capsules);
                            }
                        }
                    }
                }
            }
        });

        Self { tx, search_gen }
    }

    pub fn request_search(&self, query: String) -> u64 {
        let next_gen = self.search_gen.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = self.tx.send(IoCommand::SearchSessions {
            gen_id: next_gen,
            query,
        });
        next_gen
    }

    pub fn current_search_gen(&self) -> u64 {
        self.search_gen.load(Ordering::Acquire)
    }

    pub fn send(&self, cmd: IoCommand) {
        let _ = self.tx.send(cmd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_gen_increments_monotonically() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let temp_dir = std::env::temp_dir().join(format!("fugue_io_test_{}", UuidV7::new_now()));
        let store = Arc::new(SessionSandboxStore::open_at(temp_dir.clone()).unwrap());
        let handle = AsyncIoHandle::spawn(rt.handle(), store, |_, _, _| {});
        let g1 = handle.request_search("alpha".to_string());
        let g2 = handle.request_search("beta".to_string());
        assert!(g2 > g1);
        assert_eq!(handle.current_search_gen(), g2);
        let _ = std::fs::remove_dir_all(temp_dir);
    }
}
