//! A single real language server process, shared by zero or more clients.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::io::BufReader;
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::config::ServerConfig;
use crate::framing::{read_message, write_message};
use crate::msg;
use crate::text::Text;

/// Identity of a backend. Two clients share a process only if these agree,
/// except that roots also match by containment: see [`BackendKey::covers`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BackendKey {
    pub server: String,
    /// Canonicalised, sorted, deduplicated workspace roots.
    pub roots: Vec<String>,
    /// Hash of `initializationOptions`: different settings cannot share an index.
    pub init_hash: String,
    /// Extra arguments the client passed to the shim. Clients that invoke the
    /// server differently get different processes, since the flags may change
    /// its behaviour.
    pub extra_args: Vec<String>,
}

impl BackendKey {
    pub fn new(server: &str, roots: Vec<String>, init_options: Option<&Value>) -> Self {
        Self::with_args(server, roots, init_options, Vec::new())
    }

    pub fn with_args(
        server: &str,
        mut roots: Vec<String>,
        init_options: Option<&Value>,
        extra_args: Vec<String>,
    ) -> Self {
        roots.sort();
        roots.dedup();
        let init_hash = match init_options {
            Some(v) if !v.is_null() => {
                let mut h = Sha256::new();
                h.update(canonical_json(v).as_bytes());
                format!("{:x}", h.finalize())[..16].to_string()
            }
            _ => "default".to_string(),
        };
        Self {
            server: server.to_string(),
            roots,
            init_hash,
            extra_args,
        }
    }

    /// True if a backend opened for `self` can also serve a client asking for
    /// `other`. Containment, not equality: a subset of already-indexed roots is
    /// free to serve, a superset is not.
    pub fn covers(&self, other: &BackendKey) -> bool {
        self.server == other.server
            && self.init_hash == other.init_hash
            && self.extra_args == other.extra_args
            && other.roots.iter().all(|r| {
                self.roots
                    .iter()
                    .any(|mine| r == mine || r.starts_with(&format!("{mine}/")))
            })
    }

    pub fn label(&self) -> String {
        format!("{} [{}]", self.server, self.roots.join(", "))
    }
}

/// The `PATH` a real language server is spawned with, minus the directory
/// holding drey's wrappers.
///
/// Configuring an absolute path to the real binary is not enough on its own.
/// Version managers install *proxies* under those absolute paths: asdf's
/// `rust-analyzer` is a script that looks `rust-analyzer` up on `PATH` and
/// execs it. With the wrapper directory still on `PATH`, that resolves straight
/// back to drey, and the daemon forkbombs itself. Removing the directory from
/// the child's environment defeats the whole class of proxy, not just asdf's.
fn sanitised_path() -> String {
    let mut banned: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            banned.push(dir.to_path_buf());
        }
    }
    if let Some(home) = dirs::home_dir() {
        banned.push(home.join(".drey/bin"));
    }

    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|entry| {
            let p = std::path::Path::new(entry);
            !banned.iter().any(|b| b == p)
        })
        .collect::<Vec<_>>()
        .join(":")
}

/// True if `command` resolves to something inside drey's wrapper directory.
fn is_own_wrapper(command: &str) -> bool {
    let path = std::path::Path::new(command);
    let Some(parent) = path.parent() else {
        return false;
    };
    if parent.as_os_str().is_empty() {
        return false; // A bare name; PATH sanitising covers this case.
    }
    let mut banned: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            banned.push(dir.to_path_buf());
        }
    }
    if let Some(home) = dirs::home_dir() {
        banned.push(home.join(".drey/bin"));
    }
    banned.iter().any(|b| b == parent)
}

/// Key-order-independent rendering, so equivalent settings hash equally.
fn canonical_json(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            let inner: Vec<String> = keys
                .iter()
                .map(|k| format!("{:?}:{}", k, canonical_json(&map[*k])))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(canonical_json).collect::<Vec<_>>().join(",")
        ),
        other => other.to_string(),
    }
}

/// One open document, in two versions.
///
/// `base` is the text every client agrees on, which for agent workloads is
/// almost always what is on disk. `loaded` is what the server actually holds
/// right now, which is `base` overlaid with one client's unsaved edits.
///
/// Keeping them apart is what lets divergent clients share a process: instead
/// of forking a whole second server, drey swaps `loaded` between clients.
pub struct SharedDoc {
    pub base: Text,
    pub loaded: Text,
    pub version: i64,
    pub refs: HashSet<u64>,
}

impl SharedDoc {
    /// True while no client has unsaved edits, so everyone can be served at
    /// once with no swapping.
    pub fn is_clean(&self) -> bool {
        self.base == self.loaded
    }
}

#[derive(Default)]
pub struct BackendState {
    pub docs: HashMap<String, SharedDoc>,
    pub clients: HashMap<u64, mpsc::UnboundedSender<Vec<u8>>>,
    /// Server-initiated requests that arrived before any client was attached.
    ///
    /// This window is real and it matters: we send `initialized` during spawn,
    /// and rust-analyzer answers it by immediately pulling its settings with
    /// `workspace/configuration`. Refusing that would leave the server running
    /// on defaults for its whole life, which is exactly the memory behaviour
    /// this proxy exists to avoid. So hold them until someone can answer.
    pub pending_requests: Vec<Value>,
    /// Which client's overlay the server currently holds. `None` means every
    /// document is at its base text, which serves all clients at once.
    pub loaded_for: Option<u64>,
    /// How many times we have swapped state. Used to spot thrashing, and to
    /// decide who diagnostics belong to.
    pub generation: u64,
    /// Set when the child dies, so sessions fail fast instead of hanging.
    pub dead: bool,
}

pub struct Backend {
    pub key: BackendKey,
    /// `Some(client_id)` for a forked backend serving exactly one client.
    pub private_to: Option<u64>,
    pub init_result: Value,
    pub pid: u32,
    pub started: Instant,
    pub state: Mutex<BackendState>,
    /// When the last client detached, for idle eviction.
    pub idle_since: Mutex<Option<Instant>>,
    to_server: mpsc::UnboundedSender<Vec<u8>>,
    next_internal: AtomicU64,
}

impl Backend {
    /// Spawns the server and completes the initialize handshake.
    pub async fn spawn(
        cfg: &ServerConfig,
        key: BackendKey,
        private_to: Option<u64>,
        init_params: Value,
    ) -> Result<Arc<Self>> {
        // Refuse to run one of our own wrappers. Without this, a misconfigured
        // `command` makes the daemon spawn itself without limit.
        if is_own_wrapper(&cfg.command) {
            anyhow::bail!(
                "`{}` points at a drey wrapper, which would recurse; \
                 set `command` to the real server binary",
                cfg.command
            );
        }

        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .args(&key.extra_args)
            .env("PATH", sanitised_path())
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(root) = key.roots.first() {
            cmd.current_dir(root);
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning `{}`", cfg.command))?;
        let pid = child.id().unwrap_or(0);
        let mut stdin = child.stdin.take().expect("piped");
        let mut stdout = BufReader::new(child.stdout.take().expect("piped"));
        let stderr = child.stderr.take().expect("piped");

        // Server stderr is diagnostics for us, never protocol. Keep it out of
        // every client's stream and in our log.
        let label = key.label();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(server = %label, "{line}");
            }
        });

        // The handshake runs before the router starts, so we can read the
        // response directly instead of racing the router for it.
        let init_id = json!(format!("{}initialize", msg::INTERNAL));
        write_message(
            &mut stdin,
            &msg::encode(&msg::request(init_id.clone(), "initialize", init_params)),
        )
        .await?;

        let init_result = loop {
            let Some(raw) = read_message(&mut stdout).await? else {
                anyhow::bail!("`{}` exited during initialize", cfg.command);
            };
            let v: Value = serde_json::from_slice(&raw)?;
            if v.get("id") == Some(&init_id) {
                if let Some(err) = v.get("error") {
                    anyhow::bail!("`{}` failed to initialize: {err}", cfg.command);
                }
                break v.get("result").cloned().unwrap_or(Value::Null);
            }
            // Servers may log before responding. Nothing is attached yet, so
            // there is nobody to deliver it to.
        };

        write_message(
            &mut stdin,
            &msg::encode(&msg::notification("initialized", json!({}))),
        )
        .await?;

        let (to_server, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let backend = Arc::new(Self {
            key,
            private_to,
            init_result,
            pid,
            started: Instant::now(),
            state: Mutex::new(BackendState::default()),
            idle_since: Mutex::new(Some(Instant::now())),
            to_server,
            next_internal: AtomicU64::new(0),
        });

        tokio::spawn(async move {
            while let Some(bytes) = rx.recv().await {
                if write_message(&mut stdin, &bytes).await.is_err() {
                    break;
                }
            }
        });

        let router = backend.clone();
        tokio::spawn(async move {
            loop {
                match read_message(&mut stdout).await {
                    Ok(Some(raw)) => match serde_json::from_slice::<Value>(&raw) {
                        Ok(v) => router.route_from_server(v),
                        Err(e) => tracing::warn!("unparseable message from server: {e}"),
                    },
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!("read error from server: {e}");
                        break;
                    }
                }
            }
            tracing::warn!(server = %router.key.label(), "language server exited");
            router.mark_dead();
            let _ = child.start_kill();
        });

        Ok(backend)
    }

    pub fn send(&self, v: &Value) {
        let _ = self.to_server.send(msg::encode(v));
    }

    pub fn is_dead(&self) -> bool {
        self.state.lock().unwrap().dead
    }

    fn mark_dead(&self) {
        let mut st = self.state.lock().unwrap();
        st.dead = true;
        // Waking every client lets their sessions notice and restart cleanly
        // rather than waiting forever on a reply that is never coming.
        for tx in st.clients.values() {
            let _ = tx.send(msg::encode(&msg::notification(
                "window/logMessage",
                json!({ "type": 1, "message": "drey: language server exited" }),
            )));
        }
        st.clients.clear();
    }

    pub fn internal_id(&self) -> Value {
        let n = self.next_internal.fetch_add(1, Ordering::Relaxed);
        json!(format!("{}{n}", msg::INTERNAL))
    }

    pub fn client_count(&self) -> usize {
        self.state.lock().unwrap().clients.len()
    }

    /// Decides where a message from the server goes.
    fn route_from_server(&self, v: Value) {
        let st = self.state.lock().unwrap();

        if msg::is_response(&v) {
            let id = &v["id"];
            if msg::is_internal_id(id) {
                return; // A reply to something we asked on our own behalf.
            }
            if let Some((client, original)) = msg::decode_id(id) {
                let mut out = v;
                out["id"] = original;
                if let Some(tx) = st.clients.get(&client) {
                    let _ = tx.send(msg::encode(&out));
                }
            }
            return;
        }

        if msg::is_request(&v) {
            // Server-to-client requests are the thing other multiplexers drop.
            // They matter: rust-analyzer pulls its settings through
            // `workspace/configuration`, so dropping them silently runs the
            // server on defaults. Exactly one client may answer, or the server
            // sees duplicate replies to one id. Lowest client id is a stable
            // choice and survives other clients coming and going.
            if let Some((_, tx)) = st.clients.iter().min_by_key(|(id, _)| **id) {
                let _ = tx.send(msg::encode(&v));
            } else {
                let mut st = st;
                // Cap the queue so a server that talks to nobody forever cannot
                // grow the daemon without bound.
                if st.pending_requests.len() < 64 {
                    tracing::debug!(
                        method = msg::method(&v),
                        "holding server request until a client attaches"
                    );
                    st.pending_requests.push(v);
                } else {
                    self.send(&msg::error_response(
                        v["id"].clone(),
                        -32603,
                        "drey: no client attached to answer",
                    ));
                }
            }
            return;
        }

        // Notification. Diagnostics belong only to clients holding that
        // document; everything else is broadcast.
        let bytes = msg::encode(&v);
        if msg::method(&v) == "textDocument/publishDiagnostics" {
            let uri = v
                .get("params")
                .and_then(|p| p.get("uri"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if let Some(doc) = st.docs.get(uri) {
                if doc.is_clean() {
                    // Everyone agrees on this file, so the diagnostics are
                    // true for all of them.
                    for id in &doc.refs {
                        if let Some(tx) = st.clients.get(id) {
                            let _ = tx.send(bytes.clone());
                        }
                    }
                } else if let Some(owner) = st.loaded_for {
                    // These were computed against one client's unsaved edits.
                    // Sending them to anyone else would report errors about
                    // text that client never wrote. The others get their own
                    // when their state is next loaded.
                    if let Some(tx) = st.clients.get(&owner) {
                        let _ = tx.send(bytes.clone());
                    }
                }
                return;
            }
            // Unknown document (server volunteering diagnostics for a file
            // nobody opened): fall through and broadcast.
        }
        for tx in st.clients.values() {
            let _ = tx.send(bytes.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `PATH` is process-wide, so the tests that rewrite it take turns.
    static PATH_ENV: Mutex<()> = Mutex::new(());

    fn key(roots: &[&str]) -> BackendKey {
        BackendKey::new(
            "rust-analyzer",
            roots.iter().map(|s| s.to_string()).collect(),
            None,
        )
    }

    #[test]
    fn a_backend_covers_a_subset_of_its_roots() {
        assert!(key(&["/w"]).covers(&key(&["/w"])));
        assert!(key(&["/w"]).covers(&key(&["/w/crates/a"])));
        assert!(key(&["/w", "/x"]).covers(&key(&["/x"])));
    }

    #[test]
    fn a_backend_does_not_cover_roots_it_never_indexed() {
        assert!(!key(&["/w/crates/a"]).covers(&key(&["/w"])));
        assert!(!key(&["/w"]).covers(&key(&["/w", "/other"])));
        // Sibling worktrees must never be merged, despite the shared prefix.
        assert!(!key(&["/w-main"]).covers(&key(&["/w-main-2"])));
    }

    #[test]
    fn different_servers_or_settings_never_share() {
        let a = BackendKey::new("gopls", vec!["/w".into()], None);
        assert!(!key(&["/w"]).covers(&a));

        let opts = json!({ "cargo": { "features": "all" } });
        let with = BackendKey::new("rust-analyzer", vec!["/w".into()], Some(&opts));
        assert!(!key(&["/w"]).covers(&with));
    }

    #[test]
    fn settings_hash_ignores_key_order() {
        let a = json!({ "x": 1, "y": { "a": true, "b": false } });
        let b = json!({ "y": { "b": false, "a": true }, "x": 1 });
        assert_eq!(
            BackendKey::new("rs", vec![], Some(&a)).init_hash,
            BackendKey::new("rs", vec![], Some(&b)).init_hash
        );
    }

    #[test]
    fn our_own_wrapper_directory_is_stripped_from_the_child_path() {
        // Version-manager proxies re-resolve the server name from PATH. If our
        // wrapper directory survived, the daemon would spawn itself forever.
        let _guard = PATH_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let home = dirs::home_dir().unwrap();
        let wrappers = home.join(".drey/bin");
        // Other tests spawn `python3` off PATH, so put it back as we found it.
        let original = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:/usr/bin:/bin", wrappers.display()));
        let sanitised = sanitised_path();
        std::env::set_var("PATH", &original);
        assert!(!sanitised
            .split(':')
            .any(|p| std::path::Path::new(p) == wrappers));
        assert_eq!(
            sanitised, "/usr/bin:/bin",
            "only our own entry may be removed"
        );
    }

    #[test]
    fn spawning_a_wrapper_is_refused() {
        let home = dirs::home_dir().unwrap();
        let wrapper = home.join(".drey/bin/rust-analyzer");
        assert!(is_own_wrapper(&wrapper.to_string_lossy()));
        assert!(!is_own_wrapper("/usr/local/bin/rust-analyzer"));
        assert!(!is_own_wrapper("rust-analyzer"));
    }

    #[test]
    fn extra_args_split_backends_because_flags_change_behaviour() {
        let plain = BackendKey::with_args("rs", vec!["/w".into()], None, vec![]);
        let flagged = BackendKey::with_args("rs", vec!["/w".into()], None, vec!["--check".into()]);
        assert!(!plain.covers(&flagged));
        assert!(!flagged.covers(&plain));
        assert!(flagged.covers(&flagged.clone()));
    }

    #[test]
    fn argument_order_is_significant() {
        let a = BackendKey::with_args("rs", vec![], None, vec!["-a".into(), "-b".into()]);
        let b = BackendKey::with_args("rs", vec![], None, vec!["-b".into(), "-a".into()]);
        assert!(!a.covers(&b));
    }

    #[test]
    fn a_rootless_key_is_covered_by_anything_of_the_same_shape() {
        // No roots means no containment requirement, so such a client can join
        // any backend for the same server and settings.
        assert!(key(&["/w"]).covers(&key(&[])));
        assert!(!key(&[]).covers(&key(&["/w"])));
    }

    #[test]
    fn containment_is_by_path_component_not_string_prefix() {
        assert!(!key(&["/w/a"]).covers(&key(&["/w/ab"])));
        assert!(key(&["/w/a"]).covers(&key(&["/w/a/b"])));
        assert!(key(&["/w/a"]).covers(&key(&["/w/a/"])));
    }

    #[test]
    fn covering_is_reflexive_and_transitive_along_a_path() {
        let (outer, mid, inner) = (key(&["/w"]), key(&["/w/a"]), key(&["/w/a/b"]));
        assert!(outer.covers(&outer));
        assert!(outer.covers(&mid) && mid.covers(&inner));
        assert!(outer.covers(&inner));
    }

    #[test]
    fn absent_and_null_settings_hash_the_same() {
        let a = BackendKey::new("rs", vec![], None);
        let b = BackendKey::new("rs", vec![], Some(&Value::Null));
        assert_eq!(a.init_hash, "default");
        assert_eq!(a, b);
    }

    #[test]
    fn different_settings_hash_differently() {
        let a = json!({ "cargo": { "features": "all" } });
        let b = json!({ "cargo": { "features": "none" } });
        assert_ne!(
            BackendKey::new("rs", vec![], Some(&a)).init_hash,
            BackendKey::new("rs", vec![], Some(&b)).init_hash
        );
    }

    #[test]
    fn canonical_json_is_order_independent_for_objects_only() {
        let a = json!({ "x": 1, "y": 2 });
        let b = json!({ "y": 2, "x": 1 });
        assert_eq!(canonical_json(&a), canonical_json(&b));
        // Array order is meaningful, so it must survive.
        assert_ne!(
            canonical_json(&json!([1, 2])),
            canonical_json(&json!([2, 1]))
        );
    }

    #[test]
    fn canonical_json_distinguishes_a_number_from_its_text() {
        assert_ne!(
            canonical_json(&json!({ "k": 1 })),
            canonical_json(&json!({ "k": "1" }))
        );
        assert_ne!(canonical_json(&json!(null)), canonical_json(&json!("null")));
    }

    #[test]
    fn the_settings_hash_is_a_short_hex_digest() {
        let h = BackendKey::new("rs", vec![], Some(&json!({ "a": 1 }))).init_hash;
        assert_eq!(h.len(), 16);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn the_label_names_the_server_and_its_roots() {
        assert_eq!(key(&["/b", "/a"]).label(), "rust-analyzer [/a, /b]");
    }

    #[test]
    fn a_document_is_clean_only_while_base_and_overlay_agree() {
        let mut doc = SharedDoc {
            base: Text::new("a".into()),
            loaded: Text::new("a".into()),
            version: 1,
            refs: HashSet::new(),
        };
        assert!(doc.is_clean());
        doc.loaded = Text::new("a-edited".into());
        assert!(!doc.is_clean());
        doc.base = doc.loaded.clone();
        assert!(doc.is_clean());
    }

    #[test]
    fn a_bare_command_name_is_not_treated_as_a_wrapper() {
        // Bare names are handled by sanitising PATH instead.
        assert!(!is_own_wrapper("gopls"));
        assert!(!is_own_wrapper(""));
    }

    #[test]
    fn sanitising_path_leaves_unrelated_entries_alone() {
        let _guard = PATH_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let original = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", "/usr/bin:/opt/x/bin");
        let sanitised = sanitised_path();
        std::env::set_var("PATH", &original);
        assert_eq!(sanitised, "/usr/bin:/opt/x/bin");
    }

    #[test]
    fn roots_are_normalised_so_ordering_does_not_split_backends() {
        let a = BackendKey::new("rs", vec!["/b".into(), "/a".into(), "/a".into()], None);
        let b = BackendKey::new("rs", vec!["/a".into(), "/b".into()], None);
        assert_eq!(a, b);
    }
}
