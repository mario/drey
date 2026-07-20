//! One connected client, and its copy-on-write view of the shared documents.

use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

use crate::daemon::backend::{Backend, BackendKey, BackendState, SharedDoc};
use crate::daemon::registry::Registry;
use crate::msg;
use crate::text::Text;

/// This client's private view of a document it has open.
struct ShadowDoc {
    text: Text,
    version: i64,
    language_id: String,
}

pub struct Session {
    pub id: u64,
    reg: Arc<Registry>,
    to_client: mpsc::UnboundedSender<Vec<u8>>,
    backend: Arc<Backend>,
    key: BackendKey,
    /// Kept so a fork can initialize its private backend identically.
    init_params: Value,
    shadow: HashMap<String, ShadowDoc>,
    /// Swaps this client has caused, and when the window started. Sustained
    /// thrashing is the one case where forking still beats switching.
    swaps: u64,
    swap_window: Instant,
}

/// A client causing more than this many state swaps inside the window is
/// promoted to its own backend. Chosen high enough that ordinary editing never
/// trips it, low enough that two clients fighting over one file stop paying
/// swap costs on every request.
const SWAP_THRASH_LIMIT: u64 = 40;
const SWAP_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

impl Session {
    pub fn new(
        id: u64,
        reg: Arc<Registry>,
        to_client: mpsc::UnboundedSender<Vec<u8>>,
        backend: Arc<Backend>,
        key: BackendKey,
        init_params: Value,
    ) -> Self {
        let s = Self {
            id,
            reg,
            to_client,
            backend,
            key,
            init_params,
            shadow: HashMap::new(),
            swaps: 0,
            swap_window: Instant::now(),
        };
        s.register_with_backend();
        s
    }

    fn register_with_backend(&self) {
        let held = {
            let mut st = self.backend.state.lock().unwrap();
            st.clients.insert(self.id, self.to_client.clone());
            // We may be the first client. Anything the server asked while
            // nobody was listening is now answerable.
            std::mem::take(&mut st.pending_requests)
        };
        *self.backend.idle_since.lock().unwrap() = None;

        for req in held {
            tracing::debug!(
                client = self.id,
                method = msg::method(&req),
                "delivering held server request"
            );
            let _ = self.to_client.send(msg::encode(&req));
        }
    }

    /// Handles one message from the client. Returns `false` when the client
    /// has asked to exit.
    pub async fn handle(&mut self, v: Value) -> Result<bool> {
        if self.backend.is_dead() {
            self.respawn_backend().await?;
        }

        let method = msg::method(&v).to_string();
        match method.as_str() {
            // The handshake already happened. A second initialize would be a
            // protocol error from the client; answer from cache regardless.
            "initialize" => {
                self.reply(msg::response(
                    v["id"].clone(),
                    self.backend.init_result.clone(),
                ));
            }
            "initialized" => {}

            // Shutdown is per-client. The shared server keeps running for
            // everyone else; the daemon's idle timer decides its fate.
            "shutdown" => {
                self.reply(msg::response(v["id"].clone(), Value::Null));
            }
            "exit" => return Ok(false),

            "textDocument/didOpen" => self.did_open(&v).await?,
            "textDocument/didChange" => self.did_change(&v).await?,
            "textDocument/didSave" => self.did_save(&v),
            "textDocument/didClose" => self.did_close(&v),

            // Widening the workspace mid-session: adopt it if the current
            // backend already covers the new roots, otherwise fork so this
            // client gets a backend that has actually indexed them.
            "workspace/didChangeWorkspaceFolders" => {
                self.did_change_workspace_folders(&v).await?;
            }

            "$/cancelRequest" => {
                let mut out = v;
                if let Some(id) = out.get("params").and_then(|p| p.get("id")) {
                    out["params"]["id"] = msg::encode_id(self.id, id);
                }
                self.backend.send(&out);
            }

            _ => {
                // A request is answered against whatever the server currently
                // holds, so our state has to be loaded before it is asked.
                // Skipped entirely while nobody has unsaved edits, which is
                // the normal case and costs one lock.
                if msg::is_request(&v) && self.anyone_diverged() {
                    self.ensure_loaded().await?;
                }
                self.forward(v)
            }
        }
        Ok(true)
    }

    fn anyone_diverged(&self) -> bool {
        let st = self.backend.state.lock().unwrap();
        st.loaded_for.is_some_and(|owner| owner != self.id)
            || st.docs.values().any(|d| !d.is_clean())
    }

    /// Client requests get their id namespaced; responses to server-initiated
    /// requests pass through untouched, since those ids are the server's own.
    fn forward(&self, mut v: Value) {
        if msg::is_request(&v) {
            v["id"] = msg::encode_id(self.id, &v["id"]);
        }
        self.backend.send(&v);
    }

    fn reply(&self, v: Value) {
        let _ = self.to_client.send(msg::encode(&v));
    }

    // -- document sync ----------------------------------------------------

    async fn did_open(&mut self, v: &Value) -> Result<()> {
        let Some(doc) = v.get("params").and_then(|p| p.get("textDocument")) else {
            return Ok(());
        };
        let uri = doc
            .get("uri")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let text = doc
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let version = doc.get("version").and_then(Value::as_i64).unwrap_or(0);
        let language_id = doc
            .get("languageId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        self.shadow.insert(
            uri.clone(),
            ShadowDoc {
                text: Text::new(text.clone()),
                version,
                language_id: language_id.clone(),
            },
        );

        let is_new = {
            let mut st = self.backend.state.lock().unwrap();
            match st.docs.get_mut(&uri) {
                None => {
                    st.docs.insert(
                        uri.clone(),
                        SharedDoc {
                            base: Text::new(text.clone()),
                            loaded: Text::new(text.clone()),
                            version,
                            refs: std::iter::once(self.id).collect(),
                        },
                    );
                    true
                }
                Some(shared) => {
                    // Already open. Join it whatever its content: if we
                    // disagree, that is an overlay, not a reason to fork.
                    shared.refs.insert(self.id);
                    false
                }
            }
        };

        if is_new {
            self.backend.send(v);
            Ok(())
        } else {
            // No-op when our text matches; otherwise loads our overlay.
            self.ensure_loaded().await
        }
    }

    async fn did_change(&mut self, v: &Value) -> Result<()> {
        let Some(params) = v.get("params") else {
            return Ok(());
        };
        let uri = params
            .get("textDocument")
            .and_then(|d| d.get("uri"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let version = params
            .get("textDocument")
            .and_then(|d| d.get("version"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let changes = params
            .get("contentChanges")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let Some(shadow) = self.shadow.get_mut(&uri) else {
            // A change to a document we never saw opened. Nothing sane to
            // shadow, so forward and let the server complain.
            self.backend.send(v);
            return Ok(());
        };

        for c in &changes {
            shadow.text.apply(c);
        }
        shadow.version = version;

        // Fast path: we are the only client holding this file, nobody else has
        // unsaved edits anywhere, and the server is already showing our state.
        // Then our edit simply advances the shared base, and we can forward the
        // client's own incremental change untouched. This is the overwhelmingly
        // common case, and it costs nothing.
        let write_through = {
            let st = self.backend.state.lock().unwrap();
            let sole_holder = st
                .docs
                .get(&uri)
                .is_some_and(|d| d.refs.len() == 1 && d.refs.contains(&self.id));
            // Every document clean means no other client has an overlay
            // loaded, so advancing this one cannot trample anybody.
            sole_holder && st.docs.values().all(|d| d.is_clean())
        };

        if write_through {
            let mut st = self.backend.state.lock().unwrap();
            if let Some(shared) = st.docs.get_mut(&uri) {
                shared.base = shadow.text.clone();
                shared.loaded = shadow.text.clone();
                shared.version = version;
            }
            // Everything is still clean, so no client owns the loaded state.
            // Leaving a stale owner here would misroute the diagnostics this
            // edit is about to produce.
            st.loaded_for = None;
            drop(st);
            self.backend.send(v);
            return Ok(());
        }

        // Otherwise this is an overlay edit. Rather than fork a whole second
        // language server, push our state to the one we share.
        self.ensure_loaded().await
    }

    /// Makes the server's view match this client's, by sending a full-text
    /// `didChange` for every document that currently differs.
    ///
    /// This is the state switch. It is cheap because the server is an
    /// incremental engine: replacing one file's text invalidates only the
    /// queries depending on that file, not the whole index. Swapping costs
    /// milliseconds of re-analysis instead of gigabytes of second index.
    async fn ensure_loaded(&mut self) -> Result<()> {
        let (messages, swapped) = {
            let mut st = self.backend.state.lock().unwrap();
            let swapped = st.loaded_for != Some(self.id);
            let mut messages = Vec::new();

            for (uri, doc) in st.docs.iter_mut() {
                // What this client believes the file says: its own shadow if
                // it has the file open, otherwise the agreed base.
                let desired = match self.shadow.get(uri) {
                    Some(shadow) => &shadow.text,
                    None => &doc.base,
                };
                if doc.loaded == *desired {
                    continue;
                }
                doc.loaded = desired.clone();
                doc.version += 1;
                messages.push(msg::notification(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": doc.version },
                        "contentChanges": [{ "text": doc.loaded.content }],
                    }),
                ));
            }

            if swapped || !messages.is_empty() {
                st.loaded_for = Some(self.id);
                st.generation += 1;
            }
            (messages, swapped)
        };

        if messages.is_empty() {
            return Ok(());
        }
        tracing::debug!(
            client = self.id,
            docs = messages.len(),
            "loading client state into the shared server"
        );
        for m in &messages {
            self.backend.send(m);
        }

        if swapped {
            self.note_swap().await?;
        }
        Ok(())
    }

    /// Sustained swapping is the one case where a private server is cheaper
    /// than sharing one. Promote a thrashing client rather than let every
    /// request pay a swap.
    async fn note_swap(&mut self) -> Result<()> {
        if self.swap_window.elapsed() > SWAP_WINDOW {
            self.swap_window = Instant::now();
            self.swaps = 0;
        }
        self.swaps += 1;
        if self.swaps > SWAP_THRASH_LIMIT {
            tracing::info!(
                client = self.id,
                swaps = self.swaps,
                "state swapping too often, forking onto a private backend"
            );
            self.swaps = 0;
            return self.fork().await;
        }
        Ok(())
    }

    /// Restores every document to its base text if this client's overlay is
    /// the one currently loaded, returning the notifications to send.
    ///
    /// Called when we stop caring about our overlay (closing a file, or going
    /// away entirely). Without it the server keeps holding text that no
    /// remaining client believes in, and every one of them pays a swap to get
    /// back to reality.
    fn plan_release(&self, st: &mut BackendState) -> Vec<Value> {
        if st.loaded_for != Some(self.id) {
            return Vec::new();
        }
        let mut messages = Vec::new();
        for (uri, doc) in st.docs.iter_mut() {
            if doc.loaded == doc.base {
                continue;
            }
            doc.loaded = doc.base.clone();
            doc.version += 1;
            messages.push(msg::notification(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": uri, "version": doc.version },
                    "contentChanges": [{ "text": doc.loaded.content }],
                }),
            ));
        }
        st.loaded_for = None;
        messages
    }

    /// A save is what collapses an overlay back into shared state.
    ///
    /// This matters more for agents than for people. An agent edits a file and
    /// writes it to disk within seconds, and at that moment its version *is*
    /// the truth every other client will read. Advancing the base here is what
    /// keeps overlays short-lived, and with them the swapping.
    fn did_save(&mut self, v: &Value) {
        let Some(uri) = v
            .get("params")
            .and_then(|p| p.get("textDocument"))
            .and_then(|d| d.get("uri"))
            .and_then(Value::as_str)
        else {
            return;
        };

        if let Some(shadow) = self.shadow.get(uri) {
            let mut st = self.backend.state.lock().unwrap();
            if let Some(shared) = st.docs.get_mut(uri) {
                // What we just wrote to disk is now what everyone agrees on.
                shared.base = shadow.text.clone();
                if shared.loaded == shared.base {
                    // No overlay is loaded any more. If ours was the only one,
                    // the shared state is clean again and nobody owns it.
                    let all_clean = st.docs.values().all(|d| d.is_clean());
                    if all_clean {
                        st.loaded_for = None;
                    }
                }
            }
        }
        self.backend.send(v);
    }

    fn did_close(&mut self, v: &Value) {
        let Some(uri) = v
            .get("params")
            .and_then(|p| p.get("textDocument"))
            .and_then(|d| d.get("uri"))
            .and_then(Value::as_str)
        else {
            return;
        };
        self.shadow.remove(uri);

        let (last_ref, released) = {
            let mut st = self.backend.state.lock().unwrap();
            let last_ref = match st.docs.get_mut(uri) {
                Some(shared) => {
                    shared.refs.remove(&self.id);
                    if shared.refs.is_empty() {
                        st.docs.remove(uri);
                        true
                    } else {
                        false
                    }
                }
                None => false,
            };
            // We no longer hold this file, so our overlay should not be what
            // the server is showing everyone else.
            let released = self.plan_release(&mut st);
            (last_ref, released)
        };

        // Only the last holder may tell the server the document is closed.
        if last_ref {
            self.backend.send(v);
        }
        for m in &released {
            self.backend.send(m);
        }
    }

    async fn did_change_workspace_folders(&mut self, v: &Value) -> Result<()> {
        let added: Vec<String> = v
            .get("params")
            .and_then(|p| p.get("event"))
            .and_then(|e| e.get("added"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|f| f.get("uri").and_then(Value::as_str))
                    .map(crate::daemon::uri_to_path)
                    .collect()
            })
            .unwrap_or_default();

        if added.is_empty() {
            self.backend.send(v);
            return Ok(());
        }

        let mut roots = self.key.roots.clone();
        roots.extend(added);
        let wider = BackendKey::new(&self.key.server, roots, None);

        if self.backend.key.covers(&wider) {
            self.key = wider;
            self.backend.send(v);
            return Ok(());
        }

        tracing::info!(
            client = self.id,
            "workspace widened beyond this backend, forking"
        );
        self.key = wider;
        self.fork().await
    }

    // -- backend switching ------------------------------------------------

    /// Moves this client onto a private backend and replays its documents.
    async fn fork(&mut self) -> Result<()> {
        let private = self
            .reg
            .fork(self.key.clone(), self.id, self.init_params.clone())
            .await?;
        self.switch_to(private);
        Ok(())
    }

    /// The shared server died. Get everyone a working one rather than leaving
    /// the client talking to a corpse.
    async fn respawn_backend(&mut self) -> Result<()> {
        tracing::warn!(client = self.id, "backend died, respawning");
        let fresh = self
            .reg
            .attach(self.key.clone(), self.init_params.clone())
            .await?;
        self.switch_to(fresh);
        Ok(())
    }

    fn switch_to(&mut self, next: Arc<Backend>) {
        self.detach_from_backend();
        self.backend = next;
        self.register_with_backend();

        // Replay our documents so the new backend sees exactly what we see.
        for (uri, doc) in &self.shadow {
            {
                let mut st = self.backend.state.lock().unwrap();
                st.docs.entry(uri.clone()).or_insert_with(|| SharedDoc {
                    base: doc.text.clone(),
                    loaded: doc.text.clone(),
                    version: doc.version,
                    refs: std::iter::once(self.id).collect(),
                });
            }
            self.backend.send(&msg::notification(
                "textDocument/didOpen",
                json!({ "textDocument": {
                    "uri": uri,
                    "languageId": doc.language_id,
                    "version": doc.version,
                    "text": doc.text.content,
                }}),
            ));
        }
    }

    fn detach_from_backend(&self) {
        let mut orphaned = Vec::new();
        let released;
        {
            let mut st = self.backend.state.lock().unwrap();
            st.clients.remove(&self.id);
            // Hand the shared state back before our documents disappear, or
            // the survivors are left looking at our unsaved edits.
            released = self.plan_release(&mut st);
            st.docs.retain(|uri, shared| {
                shared.refs.remove(&self.id);
                if shared.refs.is_empty() {
                    orphaned.push(uri.clone());
                    false
                } else {
                    true
                }
            });
            if st.clients.is_empty() {
                *self.backend.idle_since.lock().unwrap() = Some(Instant::now());
            }
        }
        for uri in orphaned {
            self.backend.send(&msg::notification(
                "textDocument/didClose",
                json!({ "textDocument": { "uri": uri } }),
            ));
        }
        for m in &released {
            self.backend.send(m);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.detach_from_backend();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::registry::test_support::{key, mock_registry};
    use tokio::sync::mpsc::UnboundedReceiver;

    const URI: &str = "file:///w/a.rs";

    struct Harness {
        reg: Arc<Registry>,
        backend: Arc<crate::daemon::backend::Backend>,
        _tmp: tempfile::TempDir,
        next_id: u64,
    }

    impl Harness {
        async fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let reg = mock_registry(600, 0);
            let backend = reg
                .attach(key(tmp.path(), &["w"]), Value::Null)
                .await
                .unwrap();
            Self {
                reg,
                backend,
                _tmp: tmp,
                next_id: 1,
            }
        }

        fn client(&mut self) -> (Session, UnboundedReceiver<Vec<u8>>) {
            let id = self.next_id;
            self.next_id += 1;
            let (tx, rx) = mpsc::unbounded_channel();
            let session = Session::new(
                id,
                self.reg.clone(),
                tx,
                self.backend.clone(),
                self.backend.key.clone(),
                Value::Null,
            );
            (session, rx)
        }

        fn doc(&self) -> (String, String, Option<u64>, usize) {
            let st = self.backend.state.lock().unwrap();
            let d = &st.docs[URI];
            (
                d.base.content.clone(),
                d.loaded.content.clone(),
                st.loaded_for,
                d.refs.len(),
            )
        }
    }

    fn open(uri: &str, text: &str) -> Value {
        msg::notification(
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": uri, "languageId": "rust", "version": 1, "text": text }}),
        )
    }

    fn full_change(uri: &str, version: i64, text: &str) -> Value {
        msg::notification(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }],
            }),
        )
    }

    /// Waits for one message the predicate accepts, so an unrelated
    /// notification cannot make a test hang or pass by accident.
    async fn recv_matching(
        rx: &mut UnboundedReceiver<Vec<u8>>,
        want: impl Fn(&Value) -> bool,
    ) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let bytes = tokio::time::timeout_at(deadline, rx.recv()).await.ok()??;
            let v: Value = serde_json::from_slice(&bytes).unwrap();
            if want(&v) {
                return Some(v);
            }
        }
    }

    #[tokio::test]
    async fn opening_a_document_registers_it_as_clean() {
        let mut h = Harness::new().await;
        let (mut a, _rx) = h.client();
        a.handle(open(URI, "fn a() {}")).await.unwrap();
        assert_eq!(h.doc(), ("fn a() {}".into(), "fn a() {}".into(), None, 1));
    }

    #[tokio::test]
    async fn two_clients_opening_the_same_file_share_one_document() {
        let mut h = Harness::new().await;
        let (mut a, _ra) = h.client();
        let (mut b, _rb) = h.client();
        a.handle(open(URI, "same")).await.unwrap();
        b.handle(open(URI, "same")).await.unwrap();
        let st = h.backend.state.lock().unwrap();
        assert_eq!(st.docs.len(), 1);
        assert_eq!(st.docs[URI].refs.len(), 2);
        assert!(st.docs[URI].is_clean(), "agreeing clients need no overlay");
        // Joining an open document claims ownership of the loaded state, even
        // though the text is identical and nothing was sent to the server.
        assert_eq!(st.generation, 1);
        assert_eq!(st.loaded_for, Some(b.id));
    }

    #[tokio::test]
    async fn a_sole_holder_edit_advances_the_shared_base() {
        let mut h = Harness::new().await;
        let (mut a, _rx) = h.client();
        a.handle(open(URI, "one")).await.unwrap();
        a.handle(full_change(URI, 2, "two")).await.unwrap();
        // Write-through: nobody owns the loaded state, so no swap is pending.
        assert_eq!(h.doc(), ("two".into(), "two".into(), None, 1));
        assert_eq!(h.backend.state.lock().unwrap().generation, 0);
    }

    #[tokio::test]
    async fn an_edit_with_a_second_holder_becomes_an_overlay() {
        let mut h = Harness::new().await;
        let (mut a, _ra) = h.client();
        let (mut b, _rb) = h.client();
        a.handle(open(URI, "shared")).await.unwrap();
        b.handle(open(URI, "shared")).await.unwrap();
        a.handle(full_change(URI, 2, "a-edit")).await.unwrap();

        let (base, loaded, owner, refs) = h.doc();
        assert_eq!(base, "shared", "the other client must still see disk text");
        assert_eq!(loaded, "a-edit");
        assert_eq!(owner, Some(a.id));
        assert_eq!(refs, 2);
        assert_eq!(h.backend.state.lock().unwrap().generation, 2);
    }

    #[tokio::test]
    async fn the_other_client_swaps_the_server_back_before_being_answered() {
        let mut h = Harness::new().await;
        let (mut a, _ra) = h.client();
        let (mut b, mut rb) = h.client();
        a.handle(open(URI, "shared")).await.unwrap();
        b.handle(open(URI, "shared")).await.unwrap();
        a.handle(full_change(URI, 2, "a-edit")).await.unwrap();

        b.handle(msg::request(json!(9), "test/ping", json!({})))
            .await
            .unwrap();
        let (base, loaded, owner, _) = h.doc();
        assert_eq!(base, "shared");
        assert_eq!(loaded, "shared", "b must be answered against its own text");
        assert_eq!(owner, Some(b.id));
        assert_eq!(h.backend.state.lock().unwrap().generation, 3);

        // And the answer really comes back, with b's own request id restored.
        let reply = recv_matching(&mut rb, |v| v["id"] == json!(9))
            .await
            .unwrap();
        assert!(reply["result"]["pid"].is_number());
    }

    #[tokio::test]
    async fn a_save_collapses_the_overlay_into_the_shared_base() {
        let mut h = Harness::new().await;
        let (mut a, _ra) = h.client();
        let (mut b, _rb) = h.client();
        a.handle(open(URI, "shared")).await.unwrap();
        b.handle(open(URI, "shared")).await.unwrap();
        a.handle(full_change(URI, 2, "a-edit")).await.unwrap();
        assert_eq!(h.doc().2, Some(a.id));

        a.handle(msg::notification(
            "textDocument/didSave",
            json!({ "textDocument": { "uri": URI } }),
        ))
        .await
        .unwrap();

        let (base, loaded, owner, _) = h.doc();
        assert_eq!(base, "a-edit", "what was written to disk is now shared");
        assert_eq!(loaded, "a-edit");
        assert_eq!(owner, None, "nobody owns clean state");
    }

    #[tokio::test]
    async fn closing_drops_only_the_last_reference() {
        let mut h = Harness::new().await;
        let (mut a, _ra) = h.client();
        let (mut b, _rb) = h.client();
        a.handle(open(URI, "x")).await.unwrap();
        b.handle(open(URI, "x")).await.unwrap();

        let close = msg::notification(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": URI } }),
        );
        a.handle(close.clone()).await.unwrap();
        assert_eq!(h.backend.state.lock().unwrap().docs[URI].refs.len(), 1);
        b.handle(close).await.unwrap();
        assert!(h.backend.state.lock().unwrap().docs.is_empty());
    }

    #[tokio::test]
    async fn closing_hands_the_shared_text_back_to_everyone_else() {
        let mut h = Harness::new().await;
        let (mut a, _ra) = h.client();
        let (mut b, _rb) = h.client();
        a.handle(open(URI, "shared")).await.unwrap();
        b.handle(open(URI, "shared")).await.unwrap();
        a.handle(full_change(URI, 2, "a-edit")).await.unwrap();

        a.handle(msg::notification(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": URI } }),
        ))
        .await
        .unwrap();

        let (base, loaded, owner, refs) = h.doc();
        assert_eq!((base.as_str(), loaded.as_str()), ("shared", "shared"));
        assert_eq!(owner, None);
        assert_eq!(refs, 1);
    }

    #[tokio::test]
    async fn a_departing_client_releases_its_overlay() {
        let mut h = Harness::new().await;
        let (mut a, _ra) = h.client();
        let (mut b, _rb) = h.client();
        a.handle(open(URI, "shared")).await.unwrap();
        b.handle(open(URI, "shared")).await.unwrap();
        a.handle(full_change(URI, 2, "a-edit")).await.unwrap();

        let a_id = a.id;
        drop(a);

        let (base, loaded, owner, refs) = h.doc();
        assert_eq!(
            loaded, base,
            "survivors must not be left reading dead edits"
        );
        assert_eq!(owner, None);
        assert_eq!(refs, 1);
        assert!(!h.backend.state.lock().unwrap().clients.contains_key(&a_id));
    }

    #[tokio::test]
    async fn the_last_client_leaving_starts_the_idle_clock() {
        let mut h = Harness::new().await;
        let (a, _ra) = h.client();
        assert!(h.backend.idle_since.lock().unwrap().is_none());
        drop(a);
        assert!(h.backend.idle_since.lock().unwrap().is_some());
        assert_eq!(h.backend.client_count(), 0);
    }

    #[tokio::test]
    async fn a_second_initialize_is_answered_from_the_cached_result() {
        let mut h = Harness::new().await;
        let (mut a, mut rx) = h.client();
        a.handle(msg::request(json!(1), "initialize", json!({})))
            .await
            .unwrap();
        let reply = recv_matching(&mut rx, |v| v["id"] == json!(1))
            .await
            .unwrap();
        assert_eq!(reply["result"], h.backend.init_result);
        assert_eq!(reply["result"]["serverInfo"]["name"], "mock");
    }

    #[tokio::test]
    async fn shutdown_is_answered_per_client_without_stopping_the_server() {
        let mut h = Harness::new().await;
        let (mut a, mut rx) = h.client();
        assert!(a
            .handle(msg::request(json!(2), "shutdown", Value::Null))
            .await
            .unwrap());
        let reply = recv_matching(&mut rx, |v| v["id"] == json!(2))
            .await
            .unwrap();
        assert_eq!(reply["result"], Value::Null);
        assert!(!h.backend.is_dead());
    }

    #[tokio::test]
    async fn exit_ends_the_session_loop() {
        let mut h = Harness::new().await;
        let (mut a, _rx) = h.client();
        assert!(!a
            .handle(msg::notification("exit", Value::Null))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn a_change_to_an_unopened_document_is_forwarded_not_shadowed() {
        let mut h = Harness::new().await;
        let (mut a, _rx) = h.client();
        a.handle(full_change("file:///w/ghost.rs", 1, "x"))
            .await
            .unwrap();
        assert!(h.backend.state.lock().unwrap().docs.is_empty());
    }

    #[tokio::test]
    async fn the_first_client_to_attach_answers_what_the_server_asked_early() {
        // The mock pulls its settings the moment it is initialized, before any
        // client exists. Dropping that request would leave it on defaults.
        let mut h = Harness::new().await;
        for _ in 0..50 {
            if !h.backend.state.lock().unwrap().pending_requests.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let (_a, mut rx) = h.client();
        let held = recv_matching(&mut rx, |v| msg::method(v) == "workspace/configuration")
            .await
            .unwrap();
        assert!(
            held.get("id").is_some(),
            "it is a request, not a notification"
        );
        assert!(h.backend.state.lock().unwrap().pending_requests.is_empty());
    }

    #[tokio::test]
    async fn diagnostics_for_an_agreed_file_reach_every_holder() {
        let mut h = Harness::new().await;
        let (mut a, mut ra) = h.client();
        let (mut b, mut rb) = h.client();
        a.handle(open(URI, "shared")).await.unwrap();
        b.handle(open(URI, "shared")).await.unwrap();

        a.handle(msg::request(
            json!(1),
            "test/diagnose",
            json!({ "uri": URI }),
        ))
        .await
        .unwrap();

        let is_diag = |v: &Value| msg::method(v) == "textDocument/publishDiagnostics";
        assert!(recv_matching(&mut ra, is_diag).await.is_some());
        assert!(recv_matching(&mut rb, is_diag).await.is_some());
    }

    #[tokio::test]
    async fn diagnostics_about_one_clients_unsaved_edits_go_only_to_that_client() {
        let mut h = Harness::new().await;
        let (mut a, mut ra) = h.client();
        let (mut b, mut rb) = h.client();
        a.handle(open(URI, "shared")).await.unwrap();
        b.handle(open(URI, "shared")).await.unwrap();
        a.handle(full_change(URI, 2, "a-edit")).await.unwrap();

        a.handle(msg::request(
            json!(1),
            "test/diagnose",
            json!({ "uri": URI }),
        ))
        .await
        .unwrap();

        let is_diag = |v: &Value| msg::method(v) == "textDocument/publishDiagnostics";
        assert!(recv_matching(&mut ra, is_diag).await.is_some());
        // b never wrote that text, so it must not be told about errors in it.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let mut leaked = false;
        while let Ok(bytes) = rb.try_recv() {
            let v: Value = serde_json::from_slice(&bytes).unwrap();
            leaked |= is_diag(&v);
        }
        assert!(!leaked, "b was told about a-edit's diagnostics");
    }

    #[tokio::test]
    async fn widening_within_the_indexed_roots_keeps_the_same_backend() {
        let mut h = Harness::new().await;
        let (mut a, _rx) = h.client();
        let inner = format!("{}/crates/a", h.backend.key.roots[0]);
        a.handle(msg::notification(
            "workspace/didChangeWorkspaceFolders",
            json!({ "event": { "added": [{ "uri": format!("file://{inner}") }] } }),
        ))
        .await
        .unwrap();
        assert_eq!(h.reg.list().len(), 1);
        assert!(a.key.roots.iter().any(|r| r == &inner));
    }

    #[tokio::test]
    async fn widening_past_the_indexed_roots_forks_a_private_backend() {
        let mut h = Harness::new().await;
        let (mut a, _rx) = h.client();
        let outside = h._tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        a.handle(msg::notification(
            "workspace/didChangeWorkspaceFolders",
            json!({ "event": { "added": [
                { "uri": format!("file://{}", outside.display()) }] } }),
        ))
        .await
        .unwrap();

        let live = h.reg.list();
        assert_eq!(live.len(), 2);
        assert_eq!(live.iter().filter(|b| b.private).count(), 1);
        assert_eq!(h.backend.client_count(), 0, "a left the shared backend");
    }

    #[tokio::test]
    async fn a_fork_replays_the_clients_open_documents() {
        let mut h = Harness::new().await;
        let (mut a, mut rx) = h.client();
        a.handle(open(URI, "carried over")).await.unwrap();

        let outside = h._tmp.path().join("outside2");
        std::fs::create_dir_all(&outside).unwrap();
        a.handle(msg::notification(
            "workspace/didChangeWorkspaceFolders",
            json!({ "event": { "added": [
                { "uri": format!("file://{}", outside.display()) }] } }),
        ))
        .await
        .unwrap();

        assert_eq!(
            h.reg.list().iter().find(|i| i.private).map(|i| i.open_docs),
            Some(1)
        );

        // Ask the private server itself: only a replayed didOpen puts the
        // document into the new process.
        a.handle(msg::request(json!(5), "test/ping", json!({})))
            .await
            .unwrap();
        let reply = recv_matching(&mut rx, |v| v["id"] == json!(5))
            .await
            .unwrap();
        assert_eq!(reply["result"]["open"], json!([URI]));
        assert_eq!(reply["result"]["texts"][URI], "carried over");
    }
}
