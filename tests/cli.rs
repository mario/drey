//! End-to-end tests that drive the real binary, using the Python mock server
//! from `tests/mock_server.py` in place of a language server.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

/// Each test gets its own runtime directory and therefore its own daemon, so
/// they neither share state nor race for the socket.
struct Env {
    tmp: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let mock = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/mock_server.py");
        std::fs::write(
            tmp.path().join("config.toml"),
            format!(
                "[server.mock]\ncommand = \"python3\"\nargs = [\"{mock}\"]\n\
                 root_markers = [\"Cargo.toml\"]\n[daemon]\nidle_timeout_secs = 0\n"
            ),
        )
        .unwrap();
        Self { tmp }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_drey"));
        c.env("DREY_CONFIG", self.tmp.path().join("config.toml"))
            .env("DREY_RUNTIME_DIR", self.tmp.path().join("run"));
        c
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.cmd().args(args).output().unwrap()
    }

    /// The daemon a test autostarts is not its child: it detaches, and nothing
    /// but `stop` ends it. Without this, every test that touches a daemon
    /// leaves one running for the life of the machine. A day of repeated runs
    /// left dozens behind here, and the suite started timing out under the
    /// process pressure.
    fn stop_daemon(&self) {
        let _ = self.cmd().arg("stop").output();
    }

    fn workspace(&self, rel: &str) -> std::path::PathBuf {
        let dir = self.tmp.path().join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}

/// An LSP client talking to a `drey serve` shim over stdio, exactly as an
/// editor would.
struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Client {
    fn start(env: &Env, cwd: &std::path::Path) -> Self {
        Self::start_as(env, cwd, "mock")
    }

    fn start_as(env: &Env, cwd: &std::path::Path, server: &str) -> Self {
        let mut child = env
            .cmd()
            .args(["serve", server])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    fn send(&mut self, v: &Value) {
        let body = serde_json::to_vec(v).unwrap();
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
        self.stdin.write_all(&body).unwrap();
        self.stdin.flush().unwrap();
    }

    fn read(&mut self) -> Value {
        let mut len = None;
        loop {
            let mut line = String::new();
            assert!(self.stdout.read_line(&mut line).unwrap() > 0, "shim closed");
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                len = Some(v.trim().parse::<usize>().unwrap());
            }
        }
        let mut buf = vec![0u8; len.expect("no Content-Length")];
        self.stdout.read_exact(&mut buf).unwrap();
        serde_json::from_slice(&buf).unwrap()
    }

    /// Answers any server-initiated request that arrives first, the way a real
    /// client would, and returns our own reply.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let v = self.read();
            if v.get("id") == Some(&json!(id)) && v.get("method").is_none() {
                return v;
            }
            if v.get("id").is_some() && v.get("method").is_some() {
                let reply = json!({ "jsonrpc": "2.0", "id": v["id"], "result": [{ "ok": true }] });
                self.send(&reply);
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn initialize(&mut self, root: &std::path::Path) -> Value {
        self.request(
            "initialize",
            json!({ "rootUri": format!("file://{}", root.display()), "capabilities": {} }),
        )
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.stop_daemon();
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn status(env: &Env) -> Vec<Value> {
    let out = env.run(&["status", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// The daemon updates its bookkeeping asynchronously, so poll rather than
/// sleep a fixed amount.
fn wait_for(env: &Env, want: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let s = status(env);
        if want(&s) {
            return s;
        }
        assert!(Instant::now() < deadline, "timed out; last status: {s:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn status_on_a_dead_daemon_reports_instead_of_starting_one() {
    let env = Env::new();
    let out = env.run(&["status"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no drey daemon is running"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!env.tmp.path().join("run/daemon.sock").exists());
}

#[test]
fn gc_and_stop_on_a_dead_daemon_do_not_start_one() {
    let env = Env::new();
    for verb in ["gc", "stop"] {
        let out = env.run(&[verb]);
        assert!(!out.status.success(), "`{verb}` should have failed");
    }
    assert!(!env.tmp.path().join("run/daemon.sock").exists());
}

#[test]
fn an_unknown_server_name_is_refused_before_anything_is_spawned() {
    let env = Env::new();
    let ws = env.workspace("ws");
    let mut child = env
        .cmd()
        .args(["serve", "nonesuch"])
        .current_dir(&ws)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    let body = serde_json::to_vec(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "rootUri": format!("file://{}", ws.display()), "capabilities": {} }
    }))
    .unwrap();
    write!(stdin, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
    stdin.write_all(&body).unwrap();
    stdin.flush().unwrap();
    // The shim only notices the daemon is gone once its own stdin is done.
    drop(stdin);

    let mut reply = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut reply)
        .unwrap();
    assert!(reply.is_empty(), "an unknown server must not be answered");
    assert!(child.wait().unwrap().success());
    // The daemon is up, but it started no backend for a name it does not know.
    assert!(status(&env).is_empty());
}

#[test]
fn a_shim_autostarts_the_daemon_and_the_server_answers_through_it() {
    let env = Env::new();
    let ws = env.workspace("ws");
    let mut c = Client::start(&env, &ws);

    let init = c.initialize(&ws);
    assert_eq!(init["result"]["serverInfo"]["name"], "mock");

    let pong = c.request("test/ping", json!({}));
    assert!(pong["result"]["pid"].is_number());

    let live = wait_for(&env, |s| s.len() == 1);
    assert_eq!(live[0]["server"], "mock");
    assert_eq!(live[0]["clients"], 1);
}

#[test]
fn two_clients_in_one_workspace_share_a_single_server_process() {
    let env = Env::new();
    let ws = env.workspace("ws");
    std::fs::write(ws.join("Cargo.toml"), "[workspace]").unwrap();
    let inner = env.workspace("ws/crates/inner");
    std::fs::write(inner.join("Cargo.toml"), "[package]").unwrap();

    let mut a = Client::start(&env, &ws);
    a.initialize(&ws);
    let pid_a = a.request("test/ping", json!({}))["result"]["pid"].clone();

    // Opens a crate *inside* the workspace: root widening should land it on
    // the same backend rather than a second index.
    let mut b = Client::start(&env, &inner);
    b.initialize(&inner);
    let pid_b = b.request("test/ping", json!({}))["result"]["pid"].clone();

    assert_eq!(pid_a, pid_b);
    let live = wait_for(&env, |s| s.len() == 1 && s[0]["clients"] == json!(2));
    assert_eq!(live.len(), 1);
}

#[test]
fn unrelated_workspaces_get_their_own_server_process() {
    let env = Env::new();
    let one = env.workspace("one");
    let two = env.workspace("two");

    let mut a = Client::start(&env, &one);
    a.initialize(&one);
    let pid_a = a.request("test/ping", json!({}))["result"]["pid"].clone();

    let mut b = Client::start(&env, &two);
    b.initialize(&two);
    let pid_b = b.request("test/ping", json!({}))["result"]["pid"].clone();

    assert_ne!(pid_a, pid_b);
    wait_for(&env, |s| s.len() == 2);
}

#[test]
fn one_clients_unsaved_edits_are_invisible_to_the_other() {
    let env = Env::new();
    let ws = env.workspace("ws");
    let uri = format!("file://{}/a.rs", ws.display());

    let mut a = Client::start(&env, &ws);
    a.initialize(&ws);
    let mut b = Client::start(&env, &ws);
    b.initialize(&ws);

    let doc = json!({ "uri": uri, "languageId": "rust", "version": 1, "text": "on disk" });
    a.notify("textDocument/didOpen", json!({ "textDocument": doc }));
    b.notify("textDocument/didOpen", json!({ "textDocument": doc }));

    a.notify(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": "a's unsaved edit" }],
        }),
    );

    // Whoever asks is answered against their own text, on one process.
    let seen_by_a = a.request("test/ping", json!({}));
    assert_eq!(seen_by_a["result"]["texts"][&uri], "a's unsaved edit");
    let seen_by_b = b.request("test/ping", json!({}));
    assert_eq!(seen_by_b["result"]["texts"][&uri], "on disk");
    assert_eq!(seen_by_a["result"]["pid"], seen_by_b["result"]["pid"]);

    let live = wait_for(&env, |s| s.len() == 1 && s[0]["open_docs"] == json!(1));
    assert!(
        live[0]["swaps"].as_u64().unwrap() > 0,
        "state must have swapped"
    );
}

#[test]
fn a_save_makes_one_clients_text_the_shared_truth() {
    let env = Env::new();
    let ws = env.workspace("ws");
    let uri = format!("file://{}/a.rs", ws.display());

    let mut a = Client::start(&env, &ws);
    a.initialize(&ws);
    let mut b = Client::start(&env, &ws);
    b.initialize(&ws);

    let doc = json!({ "uri": uri, "languageId": "rust", "version": 1, "text": "on disk" });
    a.notify("textDocument/didOpen", json!({ "textDocument": doc }));
    b.notify("textDocument/didOpen", json!({ "textDocument": doc }));
    // Both opens must land before A diverges. Each client is a separate
    // connection, so without this the daemon may process B's didOpen after the
    // save, which registers B's now-stale text against the new base and leaves
    // the document dirty. A round trip on B's own stream is the barrier.
    b.request("test/ping", json!({}));
    a.notify(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": "saved text" }],
        }),
    );
    a.notify(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": uri } }),
    );
    a.request("test/ping", json!({}));

    let live = wait_for(&env, |s| s.len() == 1 && s[0]["dirty_docs"] == json!(0));
    assert_eq!(live[0]["dirty_docs"], 0);
}

#[test]
fn a_departing_client_is_dropped_and_its_backend_becomes_collectable() {
    let env = Env::new();
    let ws = env.workspace("ws");
    let mut c = Client::start(&env, &ws);
    c.initialize(&ws);
    c.request("test/ping", json!({}));
    wait_for(&env, |s| s.len() == 1 && s[0]["clients"] == json!(1));

    drop(c);
    wait_for(&env, |s| s.len() == 1 && s[0]["clients"] == json!(0));

    // The config sets a zero idle timeout, so a sweep now releases it.
    let out = env.run(&["gc"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("released 1"));
    assert!(status(&env).is_empty());
}

#[test]
fn stop_takes_the_daemon_down_and_removes_its_socket() {
    let env = Env::new();
    let ws = env.workspace("ws");
    let mut c = Client::start(&env, &ws);
    c.initialize(&ws);
    wait_for(&env, |s| s.len() == 1);

    let out = env.run(&["stop"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("drey stopped"));

    let deadline = Instant::now() + Duration::from_secs(10);
    while env.run(&["status"]).status.success() {
        assert!(
            Instant::now() < deadline,
            "daemon still answering after stop"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn status_renders_a_table_when_json_is_not_asked_for() {
    let env = Env::new();
    let ws = env.workspace("ws");
    let mut c = Client::start(&env, &ws);
    c.initialize(&ws);
    wait_for(&env, |s| s.len() == 1);

    let out = env.run(&["status"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("SERVER") && text.contains("SWAPS"), "{text}");
    assert!(text.contains("mock"), "{text}");
}
