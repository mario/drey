//! The pool of live backends, and the policy for who shares what.

use anyhow::Result;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::daemon::backend::{Backend, BackendKey};

pub struct Registry {
    pub cfg: Config,
    backends: Mutex<Vec<Arc<Backend>>>,
}

#[derive(Debug, serde::Serialize)]
pub struct BackendInfo {
    pub server: String,
    pub roots: Vec<String>,
    pub pid: u32,
    pub clients: usize,
    pub open_docs: usize,
    /// Documents where some client holds unsaved edits. Zero means every
    /// client is served at once with no swapping.
    pub dirty_docs: usize,
    /// Total state swaps. Rising fast against a low client count means clients
    /// are fighting over the same files.
    pub swaps: u64,
    pub uptime_secs: u64,
    pub private: bool,
    pub dead: bool,
}

impl Registry {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            backends: Mutex::new(Vec::new()),
        }
    }

    /// Finds a shareable backend for `key`, or spawns one.
    ///
    /// Sharing requires an exact server and settings match plus root
    /// containment, so a client opening a crate inside an already-indexed
    /// workspace costs nothing.
    pub async fn attach(&self, key: BackendKey, init_params: Value) -> Result<Arc<Backend>> {
        if let Some(existing) = self.find_shareable(&key) {
            tracing::info!(backend = %existing.key.label(), "attaching to existing backend");
            return Ok(existing);
        }
        self.spawn(key, None, init_params).await
    }

    /// Spawns a backend dedicated to one client, used when that client's
    /// document state has diverged from the shared copy.
    pub async fn fork(
        &self,
        key: BackendKey,
        client: u64,
        init_params: Value,
    ) -> Result<Arc<Backend>> {
        tracing::info!(backend = %key.label(), client, "forking a private backend");
        self.spawn(key, Some(client), init_params).await
    }

    async fn spawn(
        &self,
        key: BackendKey,
        private_to: Option<u64>,
        init_params: Value,
    ) -> Result<Arc<Backend>> {
        let cfg = self.cfg.server(&key.server)?.clone();
        let backend = Backend::spawn(&cfg, key, private_to, init_params).await?;
        self.backends.lock().unwrap().push(backend.clone());
        self.enforce_cap();
        Ok(backend)
    }

    fn find_shareable(&self, key: &BackendKey) -> Option<Arc<Backend>> {
        let backends = self.backends.lock().unwrap();
        backends
            .iter()
            .filter(|b| b.private_to.is_none() && !b.is_dead() && b.key.covers(key))
            // Prefer the tightest match, so a client asking for one crate does
            // not get pinned to the widest workspace merely because it exists.
            .min_by_key(|b| b.key.roots.len())
            .cloned()
    }

    /// Drops dead backends and those idle past the timeout. Returns how many
    /// were released.
    pub fn gc(&self) -> usize {
        let timeout = Duration::from_secs(self.cfg.daemon.idle_timeout_secs);
        let now = Instant::now();
        let mut backends = self.backends.lock().unwrap();
        let before = backends.len();

        backends.retain(|b| {
            if b.is_dead() {
                return false;
            }
            if b.client_count() > 0 {
                return true;
            }
            match *b.idle_since.lock().unwrap() {
                Some(since) if now.duration_since(since) > timeout => {
                    tracing::info!(backend = %b.key.label(), "evicting idle backend");
                    b.send(&crate::msg::request(
                        b.internal_id(),
                        "shutdown",
                        Value::Null,
                    ));
                    b.send(&crate::msg::notification("exit", Value::Null));
                    false
                }
                _ => true,
            }
        });
        before - backends.len()
    }

    /// Enforces `max_backends` by releasing the longest-idle ones first.
    /// Backends with attached clients are never evicted.
    fn enforce_cap(&self) {
        let cap = self.cfg.daemon.max_backends;
        if cap == 0 {
            return;
        }
        let mut backends = self.backends.lock().unwrap();
        while backends.len() > cap {
            let victim = backends
                .iter()
                .enumerate()
                .filter(|(_, b)| b.client_count() == 0)
                .min_by_key(|(_, b)| *b.idle_since.lock().unwrap())
                .map(|(i, _)| i);
            let Some(i) = victim else { return }; // All busy: the cap yields.
            let b = backends.remove(i);
            tracing::info!(backend = %b.key.label(), "evicting to honour max_backends");
            b.send(&crate::msg::notification("exit", Value::Null));
        }
    }

    pub fn list(&self) -> Vec<BackendInfo> {
        self.backends
            .lock()
            .unwrap()
            .iter()
            .map(|b| {
                let st = b.state.lock().unwrap();
                BackendInfo {
                    server: b.key.server.clone(),
                    roots: b.key.roots.clone(),
                    pid: b.pid,
                    clients: st.clients.len(),
                    open_docs: st.docs.len(),
                    dirty_docs: st.docs.values().filter(|d| !d.is_clean()).count(),
                    swaps: st.generation,
                    uptime_secs: b.started.elapsed().as_secs(),
                    private: b.private_to.is_some(),
                    dead: st.dead,
                }
            })
            .collect()
    }

    pub fn shutdown_all(&self) {
        for b in self.backends.lock().unwrap().drain(..) {
            b.send(&crate::msg::request(
                b.internal_id(),
                "shutdown",
                Value::Null,
            ));
            b.send(&crate::msg::notification("exit", Value::Null));
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::config::{DaemonConfig, ServerConfig};
    use std::collections::HashMap;

    /// A registry wired to the mock language server the end-to-end test uses,
    /// so backend-pool policy can be exercised without a real toolchain.
    pub(crate) fn mock_registry(idle_timeout_secs: u64, max_backends: usize) -> Arc<Registry> {
        let mock = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/mock_server.py");
        let server = ServerConfig {
            command: "python3".to_string(),
            args: vec![mock.to_string()],
            env: HashMap::new(),
            root_markers: Vec::new(),
        };
        Arc::new(Registry::new(Config {
            server: HashMap::from([("mock".to_string(), server)]),
            daemon: DaemonConfig {
                idle_timeout_secs,
                max_backends,
            },
        }))
    }

    /// Roots must exist, since a backend is spawned with its first root as the
    /// working directory.
    pub(crate) fn key(base: &std::path::Path, roots: &[&str]) -> BackendKey {
        let made: Vec<String> = roots
            .iter()
            .map(|r| {
                let p = base.join(r);
                std::fs::create_dir_all(&p).unwrap();
                p.to_string_lossy().into_owned()
            })
            .collect();
        BackendKey::new("mock", made, None)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[tokio::test]
    async fn a_second_client_inside_the_same_workspace_reuses_the_process() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = mock_registry(600, 0);
        let first = reg
            .attach(key(tmp.path(), &["w"]), Value::Null)
            .await
            .unwrap();
        let second = reg
            .attach(key(tmp.path(), &["w/crates/a"]), Value::Null)
            .await
            .unwrap();
        assert_eq!(first.pid, second.pid);
        assert_eq!(reg.list().len(), 1);
    }

    #[tokio::test]
    async fn a_client_outside_the_indexed_roots_gets_its_own_process() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = mock_registry(600, 0);
        let first = reg
            .attach(key(tmp.path(), &["w"]), Value::Null)
            .await
            .unwrap();
        let other = reg
            .attach(key(tmp.path(), &["elsewhere"]), Value::Null)
            .await
            .unwrap();
        assert_ne!(first.pid, other.pid);
        assert_eq!(reg.list().len(), 2);
    }

    #[tokio::test]
    async fn the_tightest_matching_backend_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = mock_registry(600, 0);
        let tight = reg
            .attach(key(tmp.path(), &["w"]), Value::Null)
            .await
            .unwrap();
        // A wider request cannot reuse the narrow backend, so it spawns.
        let wide = reg
            .attach(key(tmp.path(), &["w", "x", "y"]), Value::Null)
            .await
            .unwrap();
        assert_ne!(tight.pid, wide.pid);
        // Both cover /w now; the narrower one is preferred.
        let third = reg
            .attach(key(tmp.path(), &["w"]), Value::Null)
            .await
            .unwrap();
        assert_eq!(third.pid, tight.pid);
    }

    #[tokio::test]
    async fn a_forked_backend_is_private_and_never_shared() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = mock_registry(600, 0);
        let shared = reg
            .attach(key(tmp.path(), &["w"]), Value::Null)
            .await
            .unwrap();
        let private = reg
            .fork(key(tmp.path(), &["w"]), 7, Value::Null)
            .await
            .unwrap();
        assert_ne!(shared.pid, private.pid);
        assert_eq!(private.private_to, Some(7));

        let next = reg
            .attach(key(tmp.path(), &["w"]), Value::Null)
            .await
            .unwrap();
        assert_eq!(next.pid, shared.pid);
        assert_eq!(reg.list().iter().filter(|b| b.private).count(), 1);
    }

    #[tokio::test]
    async fn an_unknown_server_name_fails_before_anything_is_spawned() {
        let reg = mock_registry(600, 0);
        let bogus = BackendKey::new("nonesuch", vec!["/w".into()], None);
        assert!(reg.attach(bogus, Value::Null).await.is_err());
        assert!(reg.list().is_empty());
    }

    #[tokio::test]
    async fn gc_releases_a_backend_no_one_is_using() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = mock_registry(0, 0);
        reg.attach(key(tmp.path(), &["w"]), Value::Null)
            .await
            .unwrap();
        // A fresh backend is idle from birth; a zero timeout makes it due.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(reg.gc(), 1);
        assert!(reg.list().is_empty());
    }

    #[tokio::test]
    async fn gc_keeps_a_backend_that_still_has_a_client() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = mock_registry(0, 0);
        let b = reg
            .attach(key(tmp.path(), &["w"]), Value::Null)
            .await
            .unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        b.state.lock().unwrap().clients.insert(1, tx);
        *b.idle_since.lock().unwrap() = None;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(reg.gc(), 0);
        assert_eq!(reg.list().len(), 1);
    }

    #[tokio::test]
    async fn gc_keeps_a_backend_that_is_still_within_its_idle_window() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = mock_registry(600, 0);
        reg.attach(key(tmp.path(), &["w"]), Value::Null)
            .await
            .unwrap();
        assert_eq!(reg.gc(), 0);
    }

    #[tokio::test]
    async fn max_backends_evicts_the_idle_ones_as_new_ones_arrive() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = mock_registry(600, 2);
        for root in ["a", "b", "c", "d"] {
            reg.attach(key(tmp.path(), &[root]), Value::Null)
                .await
                .unwrap();
        }
        assert_eq!(reg.list().len(), 2);
    }

    #[tokio::test]
    async fn the_cap_yields_rather_than_evict_a_backend_with_clients() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = mock_registry(600, 1);
        let busy = reg
            .attach(key(tmp.path(), &["a"]), Value::Null)
            .await
            .unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        busy.state.lock().unwrap().clients.insert(1, tx);
        *busy.idle_since.lock().unwrap() = None;

        reg.attach(key(tmp.path(), &["b"]), Value::Null)
            .await
            .unwrap();
        let live = reg.list();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].clients, 1);
    }

    #[tokio::test]
    async fn status_reports_what_the_backend_is_holding() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = mock_registry(600, 0);
        let b = reg
            .attach(key(tmp.path(), &["w"]), Value::Null)
            .await
            .unwrap();
        b.state.lock().unwrap().docs.insert(
            "file:///w/a.rs".to_string(),
            crate::daemon::backend::SharedDoc {
                base: crate::text::Text::new("a".into()),
                loaded: crate::text::Text::new("a-edited".into()),
                version: 2,
                refs: std::iter::once(1u64).collect(),
            },
        );
        b.state.lock().unwrap().generation = 3;

        let info = &reg.list()[0];
        assert_eq!(info.server, "mock");
        assert_eq!(
            info.roots,
            [tmp.path().join("w").to_string_lossy().into_owned()]
        );
        assert_eq!(info.open_docs, 1);
        assert_eq!(info.dirty_docs, 1);
        assert_eq!(info.swaps, 3);
        assert!(!info.private && !info.dead);
        assert!(info.pid > 0);
    }

    #[tokio::test]
    async fn shutdown_all_empties_the_pool() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = mock_registry(600, 0);
        reg.attach(key(tmp.path(), &["a"]), Value::Null)
            .await
            .unwrap();
        reg.attach(key(tmp.path(), &["b"]), Value::Null)
            .await
            .unwrap();
        reg.shutdown_all();
        assert!(reg.list().is_empty());
    }
}
