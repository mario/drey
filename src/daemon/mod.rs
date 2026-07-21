//! The daemon: accepts client connections and owns the backend pool.

pub mod backend;
pub mod registry;
pub mod session;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use crate::config::{self, Config};
use crate::framing::{read_message, write_message};
use crate::msg;
use backend::BackendKey;
use registry::Registry;
use session::Session;

/// The shim's opening message, sent before any LSP traffic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    /// Logical server name from the config, e.g. `rust-analyzer`.
    pub server: String,
    /// The shim's working directory, used when the client sends no root.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Arguments the client passed to the shim, forwarded to the real server.
    #[serde(default)]
    pub args: Vec<String>,
    /// Control connections ask a question and disconnect.
    #[serde(default)]
    pub control: Option<Control>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Control {
    Status,
    Stop,
    Gc,
}

pub const HELLO_METHOD: &str = "$/drey.hello";

static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

pub async fn run(foreground: bool) -> Result<()> {
    let cfg = Config::load()?;
    let dir = config::runtime_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let sock = config::socket_path();

    // A socket left by a crashed daemon would block binding. Only remove it if
    // nothing is listening, so we never knock over a healthy daemon.
    if sock.exists() {
        if UnixStream::connect(&sock).await.is_ok() {
            anyhow::bail!("a drey daemon is already listening on {}", sock.display());
        }
        std::fs::remove_file(&sock).ok();
    }

    let listener =
        UnixListener::bind(&sock).with_context(|| format!("binding {}", sock.display()))?;
    restrict_permissions(&sock)?;
    tracing::info!("drey listening on {}", sock.display());
    if foreground {
        eprintln!("drey listening on {}", sock.display());
    }

    let reg = Arc::new(Registry::new(cfg));

    // Idle sweep.
    let gc_reg = reg.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            gc_reg.gc();
        }
    });

    let accept = async {
        loop {
            let (stream, _) = listener.accept().await?;
            let reg = reg.clone();
            tokio::spawn(async move {
                if let Err(e) = serve_client(stream, reg).await {
                    tracing::warn!("client connection ended: {e:#}");
                }
            });
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };

    tokio::select! {
        r = accept => r?,
        _ = tokio::signal::ctrl_c() => tracing::info!("interrupted"),
    }

    reg.shutdown_all();
    std::fs::remove_file(&sock).ok();
    Ok(())
}

/// The socket is a remote-code-execution surface: whoever can write to it can
/// make the daemon spawn a configured server. Keep it to this user.
fn restrict_permissions(sock: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(sock, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

async fn serve_client(stream: UnixStream, reg: Arc<Registry>) -> Result<()> {
    let (rx_half, mut tx_half) = stream.into_split();
    let mut reader = BufReader::new(rx_half);

    let Some(raw) = read_message(&mut reader).await? else {
        return Ok(());
    };
    let first: Value = serde_json::from_slice(&raw)?;
    if msg::method(&first) != HELLO_METHOD {
        anyhow::bail!("expected {HELLO_METHOD} as the first message");
    }
    let hello: Hello =
        serde_json::from_value(first["params"].clone()).context("malformed hello")?;

    if let Some(control) = hello.control {
        let reply = match control {
            Control::Status => serde_json::to_value(reg.list())?,
            Control::Gc => serde_json::json!({ "released": reg.gc() }),
            Control::Stop => {
                reg.shutdown_all();
                serde_json::json!({ "stopped": true })
            }
        };
        write_message(&mut tx_half, &msg::encode(&reply)).await?;
        if matches!(control, Control::Stop) {
            // Give the reply a moment to land, then take the daemon down.
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                std::process::exit(0);
            });
        }
        return Ok(());
    }

    // Everything after the hello is ordinary LSP. The first message must be
    // `initialize`, which is also where we learn the workspace roots.
    let Some(raw) = read_message(&mut reader).await? else {
        return Ok(());
    };
    let init: Value = serde_json::from_slice(&raw)?;
    if msg::method(&init) != "initialize" {
        anyhow::bail!("expected initialize, got `{}`", msg::method(&init));
    }

    let client_id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);
    let params = init.get("params").cloned().unwrap_or(Value::Null);
    let server_cfg = reg.cfg.server(&hello.server)?.clone();
    let roots = resolve_roots(&params, hello.cwd.as_deref(), &server_cfg.root_markers);
    let key = BackendKey::with_args(
        &hello.server,
        roots,
        params.get("initializationOptions"),
        hello.args.clone(),
    );
    tracing::info!(client = client_id, backend = %key.label(), "client connected");

    let backend = reg.attach(key.clone(), params.clone()).await?;

    // Answer initialize from the backend's cached result: the real server was
    // initialized once, by whoever got there first.
    let (to_client, mut outbox) = mpsc::unbounded_channel::<Vec<u8>>();
    let _ = to_client.send(msg::encode(&msg::response(
        init["id"].clone(),
        backend.init_result.clone(),
    )));

    let writer = tokio::spawn(async move {
        while let Some(bytes) = outbox.recv().await {
            if write_message(&mut tx_half, &bytes).await.is_err() {
                break;
            }
        }
    });

    let mut session = Session::new(client_id, reg, to_client, backend, key, params);

    while let Some(raw) = read_message(&mut reader).await? {
        let v: Value = match serde_json::from_slice(&raw) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("unparseable message from client: {e}");
                continue;
            }
        };
        if !session.handle(v).await? {
            break;
        }
    }

    drop(session); // Detaches from the backend and releases its documents.
    writer.abort();
    tracing::info!(client = client_id, "client disconnected");
    Ok(())
}

/// Extracts workspace roots from `initialize` params, widening each to the
/// outermost directory carrying a marker so sibling crates share a backend.
fn resolve_roots(params: &Value, cwd: Option<&str>, markers: &[String]) -> Vec<String> {
    let mut raw: Vec<String> = Vec::new();

    if let Some(folders) = params.get("workspaceFolders").and_then(Value::as_array) {
        raw.extend(
            folders
                .iter()
                .filter_map(|f| f.get("uri").and_then(Value::as_str))
                .map(uri_to_path),
        );
    }
    if raw.is_empty() {
        if let Some(uri) = params.get("rootUri").and_then(Value::as_str) {
            raw.push(uri_to_path(uri));
        } else if let Some(path) = params.get("rootPath").and_then(Value::as_str) {
            raw.push(path.to_string());
        }
    }
    if raw.is_empty() {
        raw.extend(cwd.map(str::to_string));
    }

    raw.into_iter()
        .map(|p| {
            let path = PathBuf::from(&p);
            let canonical = path.canonicalize().unwrap_or(path);
            config::widen_root(&canonical, markers)
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// `file:///a/b` to `/a/b`, with percent-decoding. Non-file URIs pass through
/// so they still act as a stable key.
pub fn uri_to_path(uri: &str) -> String {
    let Some(rest) = uri.strip_prefix("file://") else {
        return uri.to_string();
    };
    // Strip an authority component if present (`file://host/path`).
    let path = match rest.find('/') {
        Some(i) => &rest[i..],
        None => rest,
    };
    percent_decode(path)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {

    /// Roots are canonicalised before they become a `BackendKey`, so two
    /// clients naming the same directory differently share one backend. The
    /// assertion is on `covers`, not on the strings: `covers` is what actually
    /// decides sharing, and it compares raw path text.
    #[test]
    fn a_root_named_via_symlink_or_dot_dot_shares_one_backend() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real/ws");
        std::fs::create_dir_all(real.join("src")).unwrap();

        let markers: Vec<String> = Vec::new();
        let key = |params: Value| {
            crate::daemon::backend::BackendKey::new(
                "mock",
                resolve_roots(&params, None, &markers),
                None,
            )
        };

        let direct = key(json!({ "rootUri": format!("file://{}", real.display()) }));
        let dotted = key(json!({
            "rootUri": format!("file://{}", real.join("src/..").display())
        }));
        assert!(
            direct.covers(&dotted) && dotted.covers(&direct),
            "`..` did not collapse: {:?} vs {:?}",
            direct.roots,
            dotted.roots
        );

        #[cfg(unix)]
        {
            let link = tmp.path().join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            let via_link = key(json!({ "rootUri": format!("file://{}", link.display()) }));
            assert!(
                direct.covers(&via_link) && via_link.covers(&direct),
                "a symlinked root forked a backend: {:?} vs {:?}",
                direct.roots,
                via_link.roots
            );
        }
    }

    use super::*;
    use serde_json::json;

    #[test]
    fn uris_decode_to_paths() {
        assert_eq!(uri_to_path("file:///a/b"), "/a/b");
        assert_eq!(uri_to_path("file:///a/my%20crate"), "/a/my crate");
        assert_eq!(uri_to_path("file://localhost/a/b"), "/a/b");
        assert_eq!(uri_to_path("untitled:1"), "untitled:1");
    }

    #[test]
    fn workspace_folders_win_over_root_uri() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        let params = json!({
            "workspaceFolders": [{ "uri": format!("file://{}", a.display()) }],
            "rootUri": "file:///ignored",
        });
        let roots = resolve_roots(&params, None, &[]);
        assert_eq!(roots.len(), 1);
        assert!(roots[0].ends_with("a"));
    }

    #[test]
    fn falls_back_to_cwd_when_the_client_sends_no_root() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy().into_owned();
        let roots = resolve_roots(&json!({}), Some(&cwd), &[]);
        assert_eq!(roots.len(), 1);
    }

    #[test]
    fn percent_decoding_handles_the_awkward_cases() {
        assert_eq!(uri_to_path("file:///a/%2Fb"), "/a//b");
        assert_eq!(uri_to_path("file:///a/%c3%a9"), "/a/é");
        assert_eq!(uri_to_path("file:///a/%C3%A9"), "/a/é");
        // Not an escape: leave it alone rather than mangling the path.
        assert_eq!(uri_to_path("file:///a/100%"), "/a/100%");
        assert_eq!(uri_to_path("file:///a/%zz"), "/a/%zz");
        assert_eq!(uri_to_path("file:///a/%2"), "/a/%2");
    }

    #[test]
    fn a_file_uri_without_a_path_still_yields_something_stable() {
        assert_eq!(uri_to_path("file://"), "");
        assert_eq!(uri_to_path("file://host"), "host");
    }

    #[test]
    fn non_file_uris_are_left_untouched_so_they_still_key_documents() {
        for uri in [
            "untitled:Untitled-1",
            "jdt://contents/rt.jar",
            "vscode-vfs://x/y",
        ] {
            assert_eq!(uri_to_path(uri), uri);
        }
    }

    #[test]
    fn every_workspace_folder_becomes_a_root() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let params = json!({ "workspaceFolders": [
            { "uri": format!("file://{}", a.display()) },
            { "uri": format!("file://{}", b.display()) },
        ]});
        assert_eq!(resolve_roots(&params, None, &[]).len(), 2);
    }

    #[test]
    fn root_path_is_honoured_when_root_uri_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let params = json!({ "rootPath": tmp.path().to_string_lossy() });
        let roots = resolve_roots(&params, Some("/ignored"), &[]);
        assert_eq!(roots.len(), 1);
        assert!(roots[0].contains(tmp.path().file_name().unwrap().to_str().unwrap()));
    }

    #[test]
    fn root_uri_beats_root_path() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        let params = json!({
            "rootUri": format!("file://{}", a.display()),
            "rootPath": "/should/not/be/used",
        });
        assert!(resolve_roots(&params, None, &[])[0].ends_with("a"));
    }

    #[test]
    fn an_empty_workspace_folder_list_falls_through_to_root_uri() {
        let tmp = tempfile::tempdir().unwrap();
        let params = json!({
            "workspaceFolders": [],
            "rootUri": format!("file://{}", tmp.path().display()),
        });
        assert_eq!(resolve_roots(&params, None, &[]).len(), 1);
    }

    #[test]
    fn a_client_with_no_root_and_no_cwd_gets_no_roots() {
        assert!(resolve_roots(&json!({}), None, &[]).is_empty());
    }

    #[test]
    fn roots_are_widened_to_the_marker_so_two_crates_agree() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let (a, b) = (ws.join("crates/a"), ws.join("crates/b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(ws.join("Cargo.toml"), "[workspace]").unwrap();
        let markers = vec!["Cargo.toml".to_string()];

        let root_of = |dir: &std::path::Path| {
            resolve_roots(
                &json!({ "rootUri": format!("file://{}", dir.display()) }),
                None,
                &markers,
            )
        };
        assert_eq!(root_of(&a), root_of(&b));
        assert!(root_of(&a)[0].ends_with("ws"));
    }

    #[test]
    fn widened_roots_produce_one_shared_backend_key() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let a = ws.join("crates/a");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(ws.join("Cargo.toml"), "[workspace]").unwrap();
        let markers = vec!["Cargo.toml".to_string()];

        let key = |dir: &std::path::Path| {
            BackendKey::with_args(
                "rust-analyzer",
                resolve_roots(
                    &json!({ "rootUri": format!("file://{}", dir.display()) }),
                    None,
                    &markers,
                ),
                None,
                Vec::new(),
            )
        };
        assert!(key(&ws).covers(&key(&a)));
        assert_eq!(key(&ws), key(&a));
    }

    #[test]
    fn a_hello_round_trips_through_the_wire_format() {
        let hello = Hello {
            server: "rust-analyzer".into(),
            cwd: Some("/w".into()),
            args: vec!["--flag".into()],
            control: Some(Control::Gc),
        };
        let note = msg::notification(HELLO_METHOD, serde_json::to_value(&hello).unwrap());
        let back: Value = serde_json::from_slice(&msg::encode(&note)).unwrap();
        assert_eq!(msg::method(&back), HELLO_METHOD);
        let parsed: Hello = serde_json::from_value(back["params"].clone()).unwrap();
        assert_eq!(parsed.server, "rust-analyzer");
        assert_eq!(parsed.args, ["--flag"]);
        assert!(matches!(parsed.control, Some(Control::Gc)));
    }

    #[test]
    fn a_hello_from_an_older_shim_still_parses() {
        // Every field but `server` is optional, so adding fields never breaks
        // a shim that predates them.
        let hello: Hello = serde_json::from_value(json!({ "server": "gopls" })).unwrap();
        assert!(hello.cwd.is_none() && hello.args.is_empty() && hello.control.is_none());
    }

    #[test]
    fn control_verbs_are_snake_case_on_the_wire() {
        assert_eq!(
            serde_json::to_value(Control::Status).unwrap(),
            json!("status")
        );
        assert_eq!(serde_json::to_value(Control::Gc).unwrap(), json!("gc"));
        assert_eq!(serde_json::to_value(Control::Stop).unwrap(), json!("stop"));
    }
}
